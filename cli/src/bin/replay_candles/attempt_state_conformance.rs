//! Cross-implementation conformance suite for [`AttemptState`].
//!
//! # Why this exists
//!
//! "What became of this entry attempt?" has **two independent
//! implementations**, and the retry gate
//! ([`trade_control_core::retry_gate::evaluate`]) branches on the answer:
//!
//! | | implementation | shape of its input |
//! |---|---|---|
//! | **live** | `compute_attempt_state` (`broker-oanda/src/oanda.rs`) | three present-tense broker snapshots (pending / open / closed) + an `OrderFate` read off the order record |
//! | **replay** | `ReplayBroker::held_attempt_state` (`replay_broker.rs`) | a held model: `resting` / `open` / `closed`, each resting order carrying an explicit `cancelled: bool` |
//!
//! They agree on five categories — pending, open, closed-win, closed-loss,
//! never-placed — and **disagreed on exactly one**: cancelled-never-filled.
//! Live inferred cancellation from `broker_trade_id.is_some()`, so an order
//! that was placed, rested, and was cancelled *before it ever filled* had no
//! trade id, fell through to `Unknown`, and `Unknown` hard-blocks re-entry for
//! the life of the plan. Replay read its `cancelled` flag and said `Cancelled`,
//! which lets the gate `continue` to the next-older attempt. One incident
//! (`PLAN-trade-142-parity-and-order-loss.md`, Gap 3), +2.63R as-designed and
//! 0R live, undetected across 60+ fixtures — because replay only ever produced
//! that state via a path live never took.
//!
//! The replay broker's own "shadow-parity assertions" compare its held model
//! against its **own** re-simulation. Nothing compared the two *implementations*.
//! That is the hole this closes.
//!
//! # Where it lives, and why here
//!
//! The replay resolver is a private method on a struct inside a **binary**'s
//! module tree (`cli/src/bin/replay_candles/`). `trade-control-cli` has no lib
//! target, so no integration test — in this crate or any other — can reach it.
//! The live resolver is a private fn in `broker-oanda`. Of the two, only the
//! live one can travel: it is a **pure function over plain data**, so
//! `broker-oanda` re-exports it through `conformance_support` behind a
//! `test-support` feature (the same pattern `trade-control-core` uses for
//! `MemStateStore`), and `cli` takes `broker-oanda` as a **dev-dependency**.
//! Nothing changes in any runtime build: the feature is off by default, and a
//! dev-dep means `cli`'s shipped binaries never link OANDA at all.
//!
//! The alternative — hoisting `ReplayBroker` into a library so an integration
//! test could see it — would move a large, deliberately binary-local simulator
//! (~1700 lines, `RefCell` interior state, `pub(crate)` seams throughout) into
//! a public API surface, purely to be observed. Exporting one pure function is
//! the smaller cut.
//!
//! # What "agree" means here
//!
//! The two resolvers are compared on the **variant** of [`AttemptState`], which
//! is precisely what the retry gate branches on
//! (`Ok(AttemptState::ClosedWin { .. }) | ... => continue`). Two payloads
//! legitimately differ and are asserted separately rather than papered over:
//!
//! - `realized_pl` — live reports the broker's real P&L; replay reports a ±1.0
//!   sentinel (it has no account ledger — see `[[replay_sizing_gap_accepted]]`).
//!   The gate reads neither. What must match is the **sign bucket**, i.e. win
//!   vs loss-or-breakeven, and that IS the variant.
//! - `broker_trade_id` — live returns OANDA's trade id (which equals the
//!   originating order id); replay mints `{order_id}-pos`. Both are opaque
//!   correlation handles the caller snapshots back onto the attempt row.
//!
//! Each row asserts the payload contract it can honestly assert, so a future
//! change that flipped a win to a loss still fails.

use broker_oanda::conformance_support::{OrderFate, compute_attempt_state};
use chrono::{DateTime, TimeZone, Utc};
use oanda_client::orders::{OrderType, PendingOrder, TimeInForce};
use oanda_client::trades::{Trade, TradeState};
use trade_control_core::broker::{AttemptState, BidAskCandle, Broker};
use trade_control_core::intent::{Intent, Shell};

use super::replay_broker::ReplayBroker;

/// The order id every scenario places under. Both sides key on it.
const ORDER_ID: &str = "ord-142";

// ---------------------------------------------------------------------------
// The scenario table
// ---------------------------------------------------------------------------

/// What became of the attempt, described in terms neither implementation owns.
///
/// Each variant is a statement about the *world*, which each side then has to
/// be told in its own vocabulary — `Situation::live_inputs` builds the OANDA
/// snapshots + `OrderFate`, `Situation::drive_replay` builds the held model.
/// Keeping the description implementation-neutral is the point: if a row could
/// only be phrased in one side's terms, it would not be testing agreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Situation {
    /// Placed; still resting at the broker, unfilled.
    RestingUnfilled,
    /// Placed; filled; the position is still open.
    FilledStillOpen,
    /// Placed; filled; closed in profit.
    ClosedInProfit,
    /// Placed; filled; closed at a loss (or scratched at breakeven).
    ClosedAtLoss,
    /// **The incident.** Placed; rested; cancelled by the retry gate's
    /// supersede path; **never filled**. It therefore never became a trade, so
    /// no `broker_trade_id` was ever snapshotted — the shape that used to fall
    /// through to `Unknown` live.
    CancelledNeverFilled,
    /// An order id nobody ever placed. The gate only asks about ids it placed,
    /// so this is a "can't happen" that must still fail safe.
    NeverPlaced,
    /// Placed, then vanished from every snapshot, and the broker cannot say
    /// what became of it (the order-record lookup failed or answered
    /// ambiguously). Genuinely unresolvable.
    ///
    /// **This row must stay `Unknown`.** `Unknown` blocking re-entry is
    /// CORRECT — it is the Bug #11 fail-safe (a still-open TradeNation
    /// position whose order id had drifted; failing open stacked a duplicate
    /// entry onto a live position). The suite exists to stop
    /// `CancelledNeverFilled` being misread as this, NOT to make this one
    /// permissive.
    UnresolvableAtBroker,
}

/// One row: a situation, the state both implementations must produce, and a
/// note on anything the row deliberately does not assert.
struct Scenario {
    name: &'static str,
    situation: Situation,
    /// The [`AttemptState`] variant both sides must return.
    expected: ExpectedState,
}

/// The expected answer, as the variant the retry gate branches on.
///
/// Payload-free by construction: see the module docs on `realized_pl` and
/// `broker_trade_id`. Rows that *can* pin a payload do so in their own
/// dedicated assertion below rather than through this enum, so this stays the
/// one thing both sides are compared on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpectedState {
    Pending,
    OpenPosition,
    ClosedWin,
    ClosedLossOrBreakeven,
    Cancelled,
    Unknown,
}

impl ExpectedState {
    /// Project an [`AttemptState`] onto its gate-relevant variant.
    fn of(state: &AttemptState) -> Self {
        match state {
            AttemptState::Pending => Self::Pending,
            AttemptState::OpenPosition { .. } => Self::OpenPosition,
            AttemptState::ClosedWin { .. } => Self::ClosedWin,
            AttemptState::ClosedLossOrBreakeven { .. } => Self::ClosedLossOrBreakeven,
            AttemptState::Cancelled => Self::Cancelled,
            AttemptState::Unknown => Self::Unknown,
        }
    }
}

/// The whole table. Case #1 is the incident.
///
/// **Every situation is expressible in both implementations** — nothing is
/// skipped. The one that came closest to not being was
/// [`Situation::UnresolvableAtBroker`]: replay has no failing order-record
/// lookup to model, so it reaches `Unknown` the only way it can — an id absent
/// from all three held lists. Live reaches it by the fate lookup failing on an
/// order absent from all three snapshots. Those are the same *statement* ("the
/// broker cannot tell us"), arrived at by each side's own route, which is what
/// a conformance row is for. It is flagged here so a reader does not mistake
/// it for an identical mechanism on both sides.
const SCENARIOS: &[Scenario] = &[
    // #1 — THE INCIDENT. Placed, rested, cancelled, never filled.
    Scenario {
        name: "cancelled-never-filled (trade-142)",
        situation: Situation::CancelledNeverFilled,
        expected: ExpectedState::Cancelled,
    },
    Scenario {
        name: "resting unfilled",
        situation: Situation::RestingUnfilled,
        expected: ExpectedState::Pending,
    },
    Scenario {
        name: "filled, still open",
        situation: Situation::FilledStillOpen,
        expected: ExpectedState::OpenPosition,
    },
    Scenario {
        name: "closed in profit",
        situation: Situation::ClosedInProfit,
        expected: ExpectedState::ClosedWin,
    },
    Scenario {
        name: "closed at a loss",
        situation: Situation::ClosedAtLoss,
        expected: ExpectedState::ClosedLossOrBreakeven,
    },
    Scenario {
        name: "never placed",
        situation: Situation::NeverPlaced,
        expected: ExpectedState::Unknown,
    },
    Scenario {
        name: "unresolvable at the broker (fail-safe)",
        situation: Situation::UnresolvableAtBroker,
        expected: ExpectedState::Unknown,
    },
];

// ---------------------------------------------------------------------------
// Live side — translate a `Situation` into OANDA snapshots + an `OrderFate`
// ---------------------------------------------------------------------------

fn oanda_pending(id: &str) -> PendingOrder {
    PendingOrder {
        id: id.into(),
        r#type: OrderType::Stop,
        instrument: "EUR_CAD".into(),
        units: "-100".into(),
        price: "1.10000".into(),
        time_in_force: TimeInForce::Gtc,
        create_time: String::new(),
        take_profit_on_fill: None,
        stop_loss_on_fill: None,
    }
}

fn oanda_trade(id: &str, state: TradeState, realized_pl: f64) -> Trade {
    Trade {
        id: id.into(),
        instrument: "EUR_CAD".into(),
        current_units: "-100".into(),
        price: 1.10000,
        open_time: String::new(),
        state,
        initial_units: "-100".into(),
        initial_margin_required: 0.0,
        margin_used: None,
        unrealized_pl: None,
        realized_pl,
        average_close_price: None,
        close_time: None,
        closing_transaction_ids: None,
        financing: 0.0,
        dividend_adjustment: 0.0,
        take_profit_order: None,
        stop_loss_order: None,
        trailing_stop_loss_order: None,
    }
}

/// Everything `lookup_attempt_state` would have fetched before handing off to
/// the pure resolver: the three present-tense snapshots, the `broker_trade_id`
/// the caller had previously snapshotted onto the attempt row (`None` until the
/// attempt reached an open position — which is exactly why a
/// cancelled-never-filled order has none), and the `OrderFate` read off the
/// order record when all three snapshots miss.
struct LiveInputs {
    pending: Vec<PendingOrder>,
    open: Vec<Trade>,
    closed: Option<Vec<Trade>>,
    trade_id: Option<&'static str>,
    fate: OrderFate,
}

impl LiveInputs {
    /// The common shape: absent from all three snapshots, no trade id ever
    /// snapshotted. Only `fate` distinguishes the rows that land here — which
    /// is the whole point of the incident: `CancelledNeverFilled` and
    /// `UnresolvableAtBroker` are indistinguishable in the snapshots and are
    /// told apart *only* by what the broker's order record says.
    fn absent(fate: OrderFate) -> Self {
        Self {
            pending: vec![],
            open: vec![],
            closed: None,
            trade_id: None,
            fate,
        }
    }
}

/// The live resolver's answer for a situation.
fn live_state(situation: Situation) -> AttemptState {
    let inputs = match situation {
        Situation::RestingUnfilled => LiveInputs {
            pending: vec![oanda_pending(ORDER_ID)],
            ..LiveInputs::absent(OrderFate::Live)
        },
        Situation::FilledStillOpen => LiveInputs {
            open: vec![oanda_trade(ORDER_ID, TradeState::Open, 0.0)],
            ..LiveInputs::absent(OrderFate::Live)
        },
        // Closed rows: the caller snapshotted the trade id while the position
        // was open, which is what makes the closed-trade scan correlatable.
        Situation::ClosedInProfit => LiveInputs {
            closed: Some(vec![oanda_trade(ORDER_ID, TradeState::Closed, 12.5)]),
            trade_id: Some(ORDER_ID),
            ..LiveInputs::absent(OrderFate::Live)
        },
        Situation::ClosedAtLoss => LiveInputs {
            closed: Some(vec![oanda_trade(ORDER_ID, TradeState::Closed, -5.0)]),
            trade_id: Some(ORDER_ID),
            ..LiveInputs::absent(OrderFate::Live)
        },
        // THE INCIDENT: absent everywhere, no trade id was ever snapshotted
        // (it never filled, so it never became a trade), and the order record
        // says it reached a terminal state without filling.
        Situation::CancelledNeverFilled => LiveInputs::absent(OrderFate::TerminallyUnfilled),
        // Never placed / unresolvable are the same shape live: absent
        // everywhere, no trade id, and no usable answer about the order.
        Situation::NeverPlaced | Situation::UnresolvableAtBroker => {
            LiveInputs::absent(OrderFate::Unresolved)
        }
    };
    compute_attempt_state(
        ORDER_ID,
        inputs.trade_id,
        &inputs.pending,
        &inputs.open,
        inputs.closed.as_deref(),
        inputs.fate,
    )
}

// ---------------------------------------------------------------------------
// Replay side — drive the held model into the same situation
// ---------------------------------------------------------------------------

/// A zero-spread bar (bid == ask == mid), so prices read plainly while still
/// exercising the bid/ask fill path. Same helper shape as the replay broker's
/// own tests.
fn bar(epoch: i64, c: f64) -> BidAskCandle {
    let (o, h, l) = (c, c + 0.0010, c - 0.0010);
    BidAskCandle {
        time: ts(epoch),
        o,
        h,
        l,
        c,
        bid_o: o,
        bid_h: h,
        bid_l: l,
        bid_c: c,
        ask_o: o,
        ask_h: h,
        ask_l: l,
        ask_c: c,
    }
}

fn ts(epoch: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(epoch, 0).single().expect("valid epoch")
}

/// A short stop-entry: trigger 1.1000, stop-loss 1.1020, take-profit 1.0950.
/// Absolute levels throughout, so resolution needs no signal geometry.
fn short_enter_intent() -> Intent {
    serde_json::from_str(
        r#"{
            "v": 1,
            "id": "t-enter",
            "not_after": "2030-01-01T00:00:00Z",
            "action": "enter",
            "instrument": "EUR/CAD",
            "direction": "short",
            "entry": { "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.1000 },
            "stop_loss": { "absolute": 1.1020 },
            "take_profit": { "absolute": 1.0950 },
            "broker": "oanda",
            "trade_id": "trade-142",
            "max_retries": 5
        }"#,
    )
    .expect("valid enter intent")
}

/// The replay resolver's answer for a situation.
///
/// Each arm builds the candle path that drives the held model into the
/// described state, then reads it as-of the last bar — the same way the replay
/// loop does (`set_as_of` then query). A resting order cannot fill on its own
/// fire bar, so every path starts with a fire bar that misses the trigger.
async fn replay_state(situation: Situation) -> AttemptState {
    // Bar 0 is always the fire/shell bar, above the 1.1000 sell-stop so nothing
    // fills on it. Later bars drive the outcome.
    let fire = bar(0, 1.1010);

    let (candles, as_of, place, cancel) = match situation {
        // Never reaches the trigger — still resting at the last bar.
        Situation::RestingUnfilled => (vec![fire, bar(3600, 1.1012)], 3600, true, false),
        // Bar 1's bid touches the 1.1000 sell-stop; nothing else happens.
        Situation::FilledStillOpen => (vec![fire, bar(3600, 1.1000)], 3600, true, false),
        // Fills on bar 1, take-profit at 1.0950 touched on bar 2.
        Situation::ClosedInProfit => (
            vec![fire, bar(3600, 1.1000), bar(7200, 1.0949)],
            7200,
            true,
            false,
        ),
        // Fills on bar 1, stop-loss at 1.1020 touched on bar 2.
        Situation::ClosedAtLoss => (
            vec![fire, bar(3600, 1.1000), bar(7200, 1.1021)],
            7200,
            true,
            false,
        ),
        // THE INCIDENT: placed, then cancelled while still resting. The price
        // path here WOULD have filled and stopped it out (bar 2 crosses both
        // the trigger and the stop) — the cancel must dominate, exactly as a
        // real cancelled order fills nothing no matter what price does next.
        Situation::CancelledNeverFilled => (
            vec![fire, bar(3600, 1.1000), bar(7200, 1.1021)],
            7200,
            true,
            true,
        ),
        // Nothing placed under this id at all.
        Situation::NeverPlaced => (vec![fire, bar(3600, 1.1010)], 3600, false, false),
        // Replay's only route to "the broker cannot tell us" is an id absent
        // from resting/open/closed — see the note on `SCENARIOS`.
        Situation::UnresolvableAtBroker => (vec![fire, bar(3600, 1.1010)], 3600, false, false),
    };

    let broker = ReplayBroker::new(candles, 0.0001);
    if place {
        broker.record_attempt(
            ORDER_ID.into(),
            short_enter_intent(),
            Shell::from_candle(&fire.mid()),
            None,
        );
    }
    if cancel {
        broker
            .cancel_order("", ORDER_ID)
            .await
            .expect("replay cancel_order never fails");
    }
    broker.set_as_of(ts(as_of));
    broker
        .lookup_attempt_state("EUR/CAD", ORDER_ID, None)
        .await
        .expect("replay lookup never fails transiently")
}

// ---------------------------------------------------------------------------
// The conformance assertion
// ---------------------------------------------------------------------------

/// **The suite.** Every scenario, both implementations, three-way agreement:
/// live == expected, replay == expected, and therefore live == replay.
///
/// Driven as one test over the table (rather than one test per row) so a
/// divergence reports *which* rows diverged together — the failure mode this
/// guards against is systemic, not a single-case typo.
#[tokio::test]
async fn both_attempt_state_resolvers_agree_on_every_scenario() {
    let mut failures: Vec<String> = Vec::new();

    for row in SCENARIOS {
        let live = ExpectedState::of(&live_state(row.situation));
        let replay = ExpectedState::of(&replay_state(row.situation).await);

        if live != row.expected {
            failures.push(format!(
                "{}: LIVE resolver said {live:?}, expected {:?}",
                row.name, row.expected
            ));
        }
        if replay != row.expected {
            failures.push(format!(
                "{}: REPLAY resolver said {replay:?}, expected {:?}",
                row.name, row.expected
            ));
        }
        if live != replay {
            failures.push(format!(
                "{}: DIVERGENCE — live {live:?} vs replay {replay:?}",
                row.name
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "AttemptState conformance failures ({} of {} scenarios affected):\n  {}",
        failures.len(),
        SCENARIOS.len(),
        failures.join("\n  ")
    );
}

/// Case #1 on its own, so the incident has a test that names it and fails
/// alone. `Cancelled` lets the retry gate `continue` to the next-older
/// attempt; `Unknown` hard-blocks re-entry for the life of the plan. That
/// single-variant difference is the whole loss.
#[tokio::test]
async fn cancelled_never_filled_is_cancelled_not_unknown_in_both() {
    let live = live_state(Situation::CancelledNeverFilled);
    let replay = replay_state(Situation::CancelledNeverFilled).await;

    assert_eq!(
        live,
        AttemptState::Cancelled,
        "live resolver: a placed-then-cancelled-never-filled order must be Cancelled. \
         Unknown here is the trade-142 loss — it hard-blocks re-entry"
    );
    assert_eq!(
        replay,
        AttemptState::Cancelled,
        "replay resolver: a cancelled resting order must be Cancelled regardless of \
         what price did afterwards"
    );
    assert_eq!(live, replay, "the two resolvers must not diverge here");
}

/// The fail-safe, pinned in both. An attempt the broker genuinely cannot
/// account for must stay `Unknown` — this is Bug #11 (a still-open TradeNation
/// position whose order id had drifted; failing open stacked a duplicate entry
/// onto a live position). A future "fix" that made `Cancelled` the catch-all
/// would make the whole table pass while re-opening that hole; this row is
/// what stops it.
#[tokio::test]
async fn unresolvable_attempt_stays_unknown_in_both() {
    assert_eq!(
        live_state(Situation::UnresolvableAtBroker),
        AttemptState::Unknown,
        "live: no snapshot hit and no usable order fate must fail SAFE"
    );
    assert_eq!(
        replay_state(Situation::UnresolvableAtBroker).await,
        AttemptState::Unknown,
        "replay: an id absent from every held list must fail SAFE"
    );
}

/// `Unknown` and `Cancelled` must stay distinguishable. The conformance table
/// compares variants, so it would be satisfied by an implementation that
/// collapsed the two — this pins that the same table's two rows land on
/// genuinely different answers on **both** sides.
#[tokio::test]
async fn cancelled_and_unknown_are_not_the_same_answer() {
    assert_ne!(
        live_state(Situation::CancelledNeverFilled),
        live_state(Situation::UnresolvableAtBroker),
        "live: collapsing Cancelled into Unknown re-creates trade-142; \
         collapsing Unknown into Cancelled re-creates Bug #11"
    );
    assert_ne!(
        replay_state(Situation::CancelledNeverFilled).await,
        replay_state(Situation::UnresolvableAtBroker).await,
        "replay: same, from the other side"
    );
}

/// The payload contracts the variant comparison deliberately does not cover.
///
/// Both sides carry payloads the retry gate ignores but a human reads, and
/// they legitimately differ (see the module docs). Asserted here so the
/// difference is *pinned* rather than merely tolerated: if replay ever started
/// reporting a real P&L, or live's sentinel drifted, this is where it shows.
#[tokio::test]
async fn payloads_differ_only_where_documented() {
    // Win: live reports the broker's real P&L, replay a +1.0 sentinel. What
    // must agree is the SIGN — that is what makes it a win on both sides.
    let (live_win, replay_win) = (
        live_state(Situation::ClosedInProfit),
        replay_state(Situation::ClosedInProfit).await,
    );
    assert_eq!(live_win, AttemptState::ClosedWin { realized_pl: 12.5 });
    assert_eq!(replay_win, AttemptState::ClosedWin { realized_pl: 1.0 });

    // Loss: same story, negative.
    let (live_loss, replay_loss) = (
        live_state(Situation::ClosedAtLoss),
        replay_state(Situation::ClosedAtLoss).await,
    );
    assert_eq!(
        live_loss,
        AttemptState::ClosedLossOrBreakeven { realized_pl: -5.0 }
    );
    assert_eq!(
        replay_loss,
        AttemptState::ClosedLossOrBreakeven { realized_pl: -1.0 }
    );

    // Open: both hand back a correlation handle, minted differently. OANDA
    // reuses the originating order id as the trade id; replay suffixes.
    assert_eq!(
        live_state(Situation::FilledStillOpen),
        AttemptState::OpenPosition {
            broker_trade_id: ORDER_ID.into()
        }
    );
    assert_eq!(
        replay_state(Situation::FilledStillOpen).await,
        AttemptState::OpenPosition {
            broker_trade_id: format!("{ORDER_ID}-pos")
        }
    );
}
