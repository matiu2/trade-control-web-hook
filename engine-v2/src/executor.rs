//! The **async executor** — the layer *above* the effects.
//!
//! [`tick_once`](crate::driver::tick_once) is pure and sync: it decides *what to
//! do* and hands back a `Vec<`[`Effect`]`>`, touching no broker. This module is the
//! **async driver a level above** that: [`Execution::drive_bar`] calls the sync tick, then
//! walks the returned effects and **executes** the acquisitive ones — awaiting the
//! [`EntryBroker`] to place an order and the [`EntryStore`] to record its outcome.
//!
//! ```text
//! drive_bar (async — owns Broker + Store)   ← the async lives HERE
//!    │  calls (sync)
//!    └─ tick_once(...) -> Vec<Effect>         ← pure decision, no broker
//!         │  produces
//!         └─ Effect::PlaceOrder { .. }        ← a description of what to do
//! ```
//!
//! The broker never appears inside `tick_once` or any [`Rule`](crate::rule::Rule):
//! the async boundary is one layer up, in the loop that owns the bar stream +
//! broker + store. This is the **async shell around a sync core** — the same shape
//! as the v1 worker/replay (`replay.rs::run` is an `async fn` that calls the sync
//! `evaluate_plan` and then `.await`s the dispatch/broker). It is the *whole* point
//! of engine-v2's design: **replay and live run the identical pure
//! [`tick_once`]** and differ only in which [`EntryBroker`]/[`EntryStore`] impls
//! this driver is handed — a live one that hits OANDA/TradeNation, or a replay one
//! that simulates fills. No mode flag is ever threaded into a rule.
//!
//! # This slice — `PlaceOrder` only
//!
//! The first executor slice wires exactly one effect end-to-end:
//! [`Effect::PlaceOrder`] → [`late_entry::resolve`](crate::late_entry::resolve) →
//! [`EntryBroker::place`] → stamp the outcome. Everything else `tick_once` already
//! handles inline (fact/scratch writes, the `Invalidate` retire stamp); this driver
//! ignores those variants. `ClosePosition` (the news-reversal-close slice) and the
//! real [`Broker`](trade_control_core::broker::Broker) adaptation (entry
//! *resolution* — trigger/SL/TP/risk) land later, on this established path.
//!
//! # Late-entry parity lives here, not in the rule
//!
//! `tick_once` already dropped a `PlaceOrder` emitted on a **stale backlog bar**
//! (its `apply` keeps acquisitive effects only when `latest_bar`). So every
//! `PlaceOrder` that *reaches* this driver is on the latest bar — but the plan may
//! still be catching up over a gap, so the placement is routed through
//! [`late_entry::resolve`](crate::late_entry::resolve): it either **places late**
//! (still resting, still valid → place now at the original trigger) or is recorded
//! **missed** (the counterfactual order already triggered in the gap). Both outcomes
//! are terminal for a single-shot enter — see [`stamp_outcome`].

use trade_control_core::intent::Direction;

use crate::effect::Effect;
use crate::facts::{EntryOutcome, FactKind, FactValue, Facts};
use crate::late_entry::{self, LateEntry, LateEntryOrder};
use crate::{Candle, EntryMechanism, TradePlan, tick_once};

/// A resolved entry order, ready to place. The v2-native, broker-agnostic shape
/// the [`EntryBroker`] receives — deliberately **decoupled** from the full
/// [`EntryRequest`](trade_control_core::broker::EntryRequest) (which needs a
/// resolved SL/TP/risk budget). Building that from the intent is entry
/// *resolution*, a later slice; this slice carries only what identifies the
/// placement so the driver + a fake broker can be exercised end-to-end.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedOrder {
    /// The instrument to trade (OANDA `EUR_USD` / TradeNation `EUR/USD`).
    pub instrument: String,
    /// Trade direction.
    pub direction: Direction,
    /// How the order rests (stop / limit / market).
    pub mechanism: EntryMechanism,
    /// The resolved resting trigger price. `None` for a market order (no resting
    /// trigger) and, in this slice, wherever the enter has not yet resolved it.
    pub trigger: Option<f64>,
}

/// Failure placing an entry. Kept minimal for the slice — the real
/// [`EntryError`](trade_control_core::broker::EntryError) taxonomy is mapped in when
/// the live broker is adapted onto this path.
#[derive(Debug, Clone, PartialEq)]
pub enum PlaceError {
    /// The broker rejected or failed the placement. Non-fatal to the plan: the
    /// enter is **not** stamped done (no `entry_outcome` written), so a later bar
    /// may retry — matching v1's "a failed placement does not poison the id".
    Rejected(String),
}

/// The broker seam the executor awaits. One method for this slice — place a
/// resolved order — returning a broker order id. `?Send` (`impl Future`, no `Send`
/// bound) to match the rest of the codebase's single-threaded executor.
///
/// A **fake** impl drives the tests; the live impl (a later slice) adapts the real
/// [`Broker`](trade_control_core::broker::Broker). The replay impl simulates a
/// fill. The driver is generic over this trait, so it is the *only* thing that
/// differs between live and replay — the tick logic is identical.
pub trait EntryBroker {
    /// Place a resolved entry order; return a broker-specific order id.
    fn place(
        &self,
        order: &PlacedOrder,
    ) -> impl core::future::Future<Output = Result<String, PlaceError>>;
}

/// The persistence seam for an entry's terminal outcome. The driver stamps the
/// outcome **both** into the in-memory [`Facts`] (so the enter's fire-once guard
/// closes on the next tick, in-process) **and** through this store (durability for
/// the live worker / replay journal). One method for the slice.
///
/// `?Send` for the same reason as [`EntryBroker`]. A fake impl records the stamps
/// in the tests; the live impl writes to Postgres, the replay impl to its journal.
pub trait EntryStore {
    /// Record that `rule_id`'s enter reached a terminal `outcome` (placed or
    /// missed). Idempotent by rule id: a single-shot enter stamps exactly once.
    fn stamp_entry_outcome(
        &self,
        rule_id: &str,
        outcome: EntryOutcomeKind,
    ) -> impl core::future::Future<Output = ()>;
}

/// The terminal outcome the driver resolves an [`Effect::PlaceOrder`] to. The
/// enter's fire-once guard only checks the fact's **presence**, so both variants
/// close the enter; the distinction is for the store / journal (and later, sizing
/// off a real fill vs a logged miss).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryOutcomeKind {
    /// The order was placed at the broker (id captured in the returned effect log,
    /// not needed by the fact). Latest-bar or a caught-up place-late.
    Placed,
    /// The counterfactual order would already have triggered in the catch-up gap —
    /// recorded missed, nothing placed. Still terminal (don't re-enter later).
    Missed,
}

/// What executing one bar's effects produced — a record for the caller's log /
/// journal, distinct from the pure `Vec<Effect>` `tick_once` returned. This slice
/// only surfaces placement outcomes; later effects extend it.
#[derive(Debug, Clone, PartialEq)]
pub struct DriveReport {
    /// One entry per acquisitive effect the driver resolved this bar.
    pub placements: Vec<PlacementReport>,
}

/// The resolution of one [`Effect::PlaceOrder`].
#[derive(Debug, Clone, PartialEq)]
pub enum PlacementReport {
    /// Placed at the broker; carries the returned order id.
    Placed { rule_id: String, order_id: String },
    /// Resolved to missed (counterfactual already triggered in the gap).
    Missed { rule_id: String },
    /// The broker rejected the placement — the enter is **not** stamped done, so a
    /// later bar may retry. Carries the reason for the log.
    Rejected { rule_id: String, reason: String },
}

/// The **execution context**: the "who executes" pair (a [`EntryBroker`] plus an
/// [`EntryStore`]) held together and threaded across the whole bar loop, distinct
/// from the per-bar tick inputs. This is where live and replay differ (a live
/// broker vs a fill simulator, a Postgres store vs a journal); the tick logic in
/// [`Execution::drive_bar`] is identical for both.
pub struct Execution<'e, B: EntryBroker, S: EntryStore> {
    /// The broker the acquisitive effects are placed against.
    pub broker: &'e B,
    /// The store terminal outcomes are recorded through.
    pub store: &'e S,
}

impl<B: EntryBroker, S: EntryStore> Execution<'_, B, S> {
    /// Drive **one** bar: tick the plan's pure rules, then execute the acquisitive
    /// effects against this context's `broker` / `store`.
    ///
    /// This is the async layer above the effects. It:
    /// 1. calls the **sync** [`tick_once`] to get this bar's effects (fact/scratch
    ///    writes are already applied to `facts` in there; the returned vec is fires
    ///    + latest-bar `PlaceOrder`s + `Invalidate`s),
    /// 2. for each [`Effect::PlaceOrder`], places directly when the `gap` is empty
    ///    (no downtime — the placement is on the latest bar) or, when catching up
    ///    over a non-empty gap, routes it through
    ///    [`late_entry::resolve`](crate::late_entry::resolve) — `broker.place().await`
    ///    on place-late, or records missed,
    /// 3. stamps the terminal outcome into `facts` **and** `store` so the enter's
    ///    fire-once guard closes.
    ///
    /// Non-`PlaceOrder` effects need no async work in this slice: `Fire` /
    /// `WriteFact` / `WriteScratch` were handled by `tick_once`, and `Invalidate`
    /// already stamped its plan-scoped retire fact there — they are ignored here.
    ///
    /// The per-bar inputs mirror [`tick_once`] plus the catch-up `gap`:
    /// - `gap` — the bars `(placement_bar, latest_bar]` the late-entry parity check
    ///   replays a resting order against (see [`late_entry`](crate::late_entry)). It
    ///   is the bars *strictly after* the one that fires the placement, so for
    ///   **normal live ticking with no downtime it is EMPTY** (the placement is on
    ///   the latest bar; nothing follows it) — an empty gap places directly,
    ///   skipping the parity check. It is non-empty only when catching up over
    ///   downtime, where the missed-vs-place-late question is real.
    pub async fn drive_bar(
        &self,
        plan: &TradePlan,
        facts: &mut Facts,
        window: &[Candle],
        now: chrono::DateTime<chrono::Utc>,
        latest_bar: bool,
        gap: &[Candle],
    ) -> DriveReport {
        // Layer below: the pure decision. This applies all fact/scratch writes and
        // the Invalidate retire stamp into `facts` in place; the vec it returns is
        // fires + acquisitive effects for us to execute.
        let effects = tick_once(plan, facts, window, now, latest_bar);

        let mut placements = Vec::new();
        for effect in effects {
            // Only PlaceOrder needs the async broker in this slice. Everything else
            // was already applied by tick_once (or is deferred to a later slice).
            if let Effect::PlaceOrder {
                fired,
                mechanism,
                trigger_price,
                ..
            } = effect
            {
                let order = PlacedOrder {
                    instrument: fired.intent.instrument.clone(),
                    direction: order_direction(plan, &fired),
                    mechanism,
                    trigger: trigger_price,
                };
                let report =
                    place_one(&fired.rule_id, &order, gap, self.broker, self.store, facts).await;
                placements.push(report);
            }
        }

        DriveReport { placements }
    }
}

/// Resolve and (maybe) place a single order, stamping its terminal outcome.
///
/// Split out of the effect loop so the parity-vs-place branching reads top-down:
/// `resolve` → missed | place-late → broker → stamp. The `facts`/`store` stamp is
/// the fire-once close the enter reads next tick.
async fn place_one<B: EntryBroker, S: EntryStore>(
    rule_id: &str,
    order: &PlacedOrder,
    gap: &[Candle],
    broker: &B,
    store: &S,
    facts: &mut Facts,
) -> PlacementReport {
    // The late-entry parity check only applies to a **catch-up backlog**. The `gap`
    // is `(placement_bar, latest_bar]` — the bars *strictly after* the bar that
    // fired this placement (see `late_entry`). On a normal live tick with no
    // downtime the placement IS on the latest bar, so there is nothing after it:
    // the gap is **empty**, and there is no counterfactual to reconstruct — place
    // directly (a market order fills now, a resting order rests now). Only when the
    // gap is non-empty (bars elapsed between a stale placement and now) do we ask
    // the missed-vs-place-late question.
    if !gap.is_empty() {
        let late = LateEntryOrder {
            mechanism: order.mechanism,
            direction: order.direction,
            trigger: order.trigger,
        };
        // Missed is terminal — stamp done, place nothing (never re-enter for a
        // signal whose counterfactual trade already played out in the gap).
        if late_entry::resolve(&late, gap) == LateEntry::Missed {
            stamp_outcome(rule_id, EntryOutcomeKind::Missed, store, facts).await;
            return PlacementReport::Missed {
                rule_id: rule_id.to_string(),
            };
        }
        // else PlaceLate: still resting and valid → fall through and place now at
        // the original trigger (exact parity with an order that never triggered).
    }

    // Place now. A broker rejection
    // does NOT stamp the enter done — a later bar may retry (v1: a failed placement
    // never poisons the id).
    match broker.place(order).await {
        Ok(order_id) => {
            stamp_outcome(rule_id, EntryOutcomeKind::Placed, store, facts).await;
            PlacementReport::Placed {
                rule_id: rule_id.to_string(),
                order_id,
            }
        }
        Err(PlaceError::Rejected(reason)) => PlacementReport::Rejected {
            rule_id: rule_id.to_string(),
            reason,
        },
    }
}

/// Stamp the terminal entry outcome in **both** places: the in-memory [`Facts`]
/// (keyed `(rule_id, "entry_outcome")` — the enter's fire-once guard reads its
/// presence) and the durable [`EntryStore`]. The fact carries no value the enter
/// inspects; `Flag(true)` is a stable presence marker.
async fn stamp_outcome<S: EntryStore>(
    rule_id: &str,
    outcome: EntryOutcomeKind,
    store: &S,
    facts: &mut Facts,
) {
    facts.set_named(rule_id, EntryOutcome::NAME, FactValue::Flag(true));
    store.stamp_entry_outcome(rule_id, outcome).await;
}

/// The trade direction for the placement. The v2 plan carries the direction at the
/// plan level; the fired intent's own `direction` is `Option` (control intents have
/// none), so prefer the intent's when set and fall back to the plan's.
fn order_direction(
    plan: &TradePlan,
    fired: &trade_control_core::plan_eval::FiredIntent,
) -> Direction {
    fired.intent.direction.unwrap_or(plan.direction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    use chrono::{DateTime, Utc};

    use trade_control_core::intent::{Action, BrokerKind, Intent};
    use trade_control_core::trade_plan::{BarEvent, CrossDir};
    use trade_control_core::tunable::Tunable;

    use crate::facts::{FactKind, PLAN_SCOPE, Paused};
    use crate::{EntryMechanism, Granularity, PlanRule, PrepMap, RuleKind};

    // --- Fakes ----------------------------------------------------------------

    /// Records every `place` call; returns a fixed order id or a rejection.
    struct FakeBroker {
        placed: RefCell<Vec<PlacedOrder>>,
        reject: bool,
    }

    impl FakeBroker {
        fn ok() -> Self {
            Self {
                placed: RefCell::new(Vec::new()),
                reject: false,
            }
        }
        fn rejecting() -> Self {
            Self {
                placed: RefCell::new(Vec::new()),
                reject: true,
            }
        }
    }

    impl EntryBroker for FakeBroker {
        async fn place(&self, order: &PlacedOrder) -> Result<String, PlaceError> {
            self.placed.borrow_mut().push(order.clone());
            if self.reject {
                Err(PlaceError::Rejected("fake-reject".into()))
            } else {
                Ok("broker-order-1".into())
            }
        }
    }

    /// Records every outcome stamp.
    #[derive(Default)]
    struct FakeStore {
        stamps: RefCell<Vec<(String, EntryOutcomeKind)>>,
    }

    impl EntryStore for FakeStore {
        async fn stamp_entry_outcome(&self, rule_id: &str, outcome: EntryOutcomeKind) {
            self.stamps
                .borrow_mut()
                .push((rule_id.to_string(), outcome));
        }
    }

    // --- Fixtures -------------------------------------------------------------

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid rfc3339")
            .with_timezone(&Utc)
    }

    fn candle(time: &str, close: f64) -> Candle {
        Candle {
            time: ts(time),
            o: close,
            h: close,
            l: close,
            c: close,
        }
    }

    fn intent() -> Intent {
        Intent {
            entry_level_vetos: Vec::new(),
            v: 1,
            id: "x".into(),
            not_before: None,
            not_after: ts("2026-06-20T00:00:00Z"),
            action: Action::Enter,
            instrument: "EUR_USD".into(),
            direction: Some(Direction::Long),
            entry: None,
            stop_loss: None,
            take_profit: None,
            risk_pct: Tunable::Static(1.0),
            risk_amount: None,
            size_units: None,
            dry_run: None,
            cooldown_hours: None,
            min_r: None,
            broker: BrokerKind::Oanda,
            account: None,
            step: None,
            name: None,
            ttl_hours: Tunable::Static(0),
            level: None,
            requires_preps: Vec::new(),
            vetos: Vec::new(),
            clears: Vec::new(),
            trade_id: None,
            max_retries: Tunable::Static(0),
            expiry_bars: None,
            allow_entry: None,
            allow_close: None,
            needs_golden: false,
            needs_confirmed: false,
            blackout_id: None,
            news_id: None,
            require_news_window: None,
            require_price_in_ranges: None,
            inside_window: Vec::new(),
            sr_bands: Vec::new(),
            veto_on_reversal: false,
            reason: None,
            mw: None,
            pip_size: None,
            tick_size: None,
            spread_window: None,
            trade_plan: None,
            blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
            breakeven: None,
            include_archived: false,
        }
    }

    /// A no-prep enter — places on the first live bar it ticks.
    fn enter_rule() -> PlanRule {
        PlanRule {
            id: "05-enter".into(),
            kind: RuleKind::Enter,
            intent: intent(),
            bar: BarEvent::OnClose,
            dir: CrossDir::Up,
            preps: PrepMap::new(),
            mechanism: EntryMechanism::Market,
        }
    }

    fn plan(rules: Vec<PlanRule>) -> TradePlan {
        TradePlan {
            trade_id: "t".into(),
            instrument: "EUR_USD".into(),
            direction: Direction::Long,
            granularity: Granularity::H1,
            lines: Vec::new(),
            levels: Vec::new(),
            markers: Vec::new(),
            pause_windows: Vec::new(),
            rules,
            cross_buffer_pct: 0.0,
            retest_atr_step: trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
        }
    }

    // --- Tests ----------------------------------------------------------------

    /// End-to-end: a no-prep enter on the latest bar places one order, stamps the
    /// outcome into Facts AND the store, and — because the enter's fire-once reads
    /// the Facts stamp — a second bar places nothing.
    #[tokio::test]
    async fn no_prep_enter_places_once_then_fire_once_closes() {
        let p = plan(vec![enter_rule()]);
        let mut facts = Facts::default();
        let broker = FakeBroker::ok();
        let store = FakeStore::default();

        let exec = Execution {
            broker: &broker,
            store: &store,
        };

        // Bar 1: latest, no downtime → empty gap → places directly.
        let bar1 = candle("2026-06-01T10:00:00Z", 1.10);
        let r1 = exec
            .drive_bar(
                &p,
                &mut facts,
                &[bar1],
                ts("2026-06-01T10:00:00Z"),
                true,
                &[],
            )
            .await;

        assert_eq!(
            r1.placements,
            vec![PlacementReport::Placed {
                rule_id: "05-enter".into(),
                order_id: "broker-order-1".into(),
            }],
            "the enter places one order on the latest bar",
        );
        assert_eq!(broker.placed.borrow().len(), 1, "broker.place called once");
        assert_eq!(
            *store.stamps.borrow(),
            vec![("05-enter".to_string(), EntryOutcomeKind::Placed)],
            "the outcome is stamped in the store",
        );
        assert!(
            facts.is_set_named("05-enter", EntryOutcome::NAME),
            "the outcome is stamped in Facts so the enter's fire-once closes",
        );

        // Bar 2: the enter is done (Facts stamp) → no placement, no new broker call.
        let bar2 = candle("2026-06-01T11:00:00Z", 1.11);
        let r2 = exec
            .drive_bar(
                &p,
                &mut facts,
                &[bar1, bar2],
                ts("2026-06-01T11:00:00Z"),
                true,
                &[],
            )
            .await;

        assert!(r2.placements.is_empty(), "fire-once: no second placement");
        assert_eq!(
            broker.placed.borrow().len(),
            1,
            "broker.place not called again"
        );
    }

    /// A broker rejection does NOT stamp the enter done — the Facts guard stays
    /// unset so a later bar retries (v1: a failed placement never poisons the id).
    #[tokio::test]
    async fn broker_rejection_leaves_enter_retryable() {
        let p = plan(vec![enter_rule()]);
        let mut facts = Facts::default();
        let broker = FakeBroker::rejecting();
        let store = FakeStore::default();

        let bar = candle("2026-06-01T10:00:00Z", 1.10);
        let r = Execution {
            broker: &broker,
            store: &store,
        }
        .drive_bar(
            &p,
            &mut facts,
            &[bar],
            ts("2026-06-01T10:00:00Z"),
            true,
            &[],
        )
        .await;

        assert!(
            matches!(r.placements.as_slice(), [PlacementReport::Rejected { .. }]),
            "the rejection is reported",
        );
        assert!(
            !facts.is_set_named("05-enter", EntryOutcome::NAME),
            "a rejected placement does NOT mark the enter done — retryable next bar",
        );
        assert!(
            store.stamps.borrow().is_empty(),
            "no outcome stamped on a rejection",
        );
    }

    /// A stale backlog bar's `PlaceOrder` never reaches the driver: `tick_once`
    /// drops acquisitive effects when `!latest_bar`, so `drive_bar` sees no
    /// placement and the broker is never called.
    #[tokio::test]
    async fn backlog_bar_places_nothing() {
        let p = plan(vec![enter_rule()]);
        let mut facts = Facts::default();
        let broker = FakeBroker::ok();
        let store = FakeStore::default();

        let bar = candle("2026-06-01T10:00:00Z", 1.10);
        let r = Execution {
            broker: &broker,
            store: &store,
        }
        .drive_bar(
            &p,
            &mut facts,
            &[bar],
            ts("2026-06-01T10:00:00Z"),
            false, // NOT the latest bar
            &[bar],
        )
        .await;

        assert!(
            r.placements.is_empty(),
            "no placement off a stale backlog bar"
        );
        assert!(broker.placed.borrow().is_empty(), "broker never called");
    }

    fn hlc(time: &str, high: f64, low: f64, close: f64) -> Candle {
        Candle {
            time: ts(time),
            o: close,
            h: high,
            l: low,
            c: close,
        }
    }

    fn stop_long(trigger: f64) -> PlacedOrder {
        PlacedOrder {
            instrument: "EUR_USD".into(),
            direction: Direction::Long,
            mechanism: EntryMechanism::Stop,
            trigger: Some(trigger),
        }
    }

    /// Catch-up: a resting stop that WOULD have triggered somewhere in a **non-empty**
    /// gap resolves to Missed — stamped done, nothing placed. Exercises `place_one`
    /// directly (the enter emits an unresolved trigger this slice), which is the unit
    /// that owns the parity branch.
    #[tokio::test]
    async fn catch_up_stop_that_would_have_triggered_is_missed() {
        let mut facts = Facts::default();
        let broker = FakeBroker::ok();
        let store = FakeStore::default();

        // Trigger 1.1050; a gap bar's high reaches 1.1060 → would have filled.
        let gap = [
            hlc("2026-06-01T11:00:00Z", 1.1040, 1.1030, 1.1035),
            hlc("2026-06-01T12:00:00Z", 1.1060, 1.1045, 1.1055),
        ];

        let r = place_one(
            "05-enter",
            &stop_long(1.1050),
            &gap,
            &broker,
            &store,
            &mut facts,
        )
        .await;

        assert_eq!(
            r,
            PlacementReport::Missed {
                rule_id: "05-enter".into()
            }
        );
        assert!(
            broker.placed.borrow().is_empty(),
            "nothing placed on a missed"
        );
        assert!(
            facts.is_set_named("05-enter", EntryOutcome::NAME),
            "Missed is terminal — the enter is stamped done",
        );
        assert_eq!(
            *store.stamps.borrow(),
            vec![("05-enter".to_string(), EntryOutcomeKind::Missed)],
        );
    }

    /// Catch-up: a resting stop that never triggered across the gap and is still on
    /// the resting side at the latest bar resolves to PlaceLate — placed now at the
    /// original trigger, outcome stamped Placed.
    #[tokio::test]
    async fn catch_up_stop_still_resting_places_late() {
        let mut facts = Facts::default();
        let broker = FakeBroker::ok();
        let store = FakeStore::default();

        // Trigger 1.1050; never reached (highs stay below), latest close 1.1035 still
        // below the stop → still resting → place late.
        let gap = [
            hlc("2026-06-01T11:00:00Z", 1.1040, 1.1030, 1.1035),
            hlc("2026-06-01T12:00:00Z", 1.1045, 1.1032, 1.1035),
        ];

        let r = place_one(
            "05-enter",
            &stop_long(1.1050),
            &gap,
            &broker,
            &store,
            &mut facts,
        )
        .await;

        assert_eq!(
            r,
            PlacementReport::Placed {
                rule_id: "05-enter".into(),
                order_id: "broker-order-1".into(),
            },
        );
        assert_eq!(
            broker.placed.borrow().len(),
            1,
            "placed late at the original trigger"
        );
        assert_eq!(
            *store.stamps.borrow(),
            vec![("05-enter".to_string(), EntryOutcomeKind::Placed)],
        );
    }

    /// A paused plan blocks the enter upstream (in `tick_once`), so the driver sees
    /// no `PlaceOrder` — the executor never has to know about the pause.
    #[tokio::test]
    async fn paused_plan_places_nothing() {
        let p = plan(vec![enter_rule()]);
        let mut facts = Facts::default();
        // Pre-set the paused flag as the Pause rule would.
        facts.set_named(PLAN_SCOPE, Paused::NAME, FactValue::Flag(true));
        let broker = FakeBroker::ok();
        let store = FakeStore::default();

        let bar = candle("2026-06-01T10:00:00Z", 1.10);
        let r = Execution {
            broker: &broker,
            store: &store,
        }
        .drive_bar(
            &p,
            &mut facts,
            &[bar],
            ts("2026-06-01T10:00:00Z"),
            true,
            &[],
        )
        .await;

        assert!(r.placements.is_empty(), "paused → no placement");
        assert!(broker.placed.borrow().is_empty(), "broker never called");
    }
}
