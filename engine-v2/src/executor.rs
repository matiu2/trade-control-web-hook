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
//! The executor wires the acquisitive [`Effect::PlaceOrder`] end-to-end:
//! **resolve** the intent into a concrete order
//! ([`resolve_order`] → v1's [`Resolved::from_intent`], via [`shell_from_candle`]) →
//! [`late_entry::resolve`](crate::late_entry::resolve) (catch-up parity) →
//! [`EntryBroker::place`] → stamp the outcome. Everything else `tick_once` already
//! handles inline (fact/scratch writes, the `Invalidate` retire stamp); this driver
//! ignores those variants.
//!
//! # Entry resolution lives here (not in the rule)
//!
//! The enter stays pure and mode-blind — it emits `PlaceOrder` with `trigger_price:
//! None` and no SL/TP/risk. The executor owns *resolution*: it reads the intent's
//! baked `pip_size`/`tick_size`, projects the firing bar (+ latched signal) onto a
//! [`Shell`](trade_control_core::intent::Shell), and runs the **same** resolver the
//! v1 worker uses ([`Resolved::from_intent`] — anchors, offsets, R-multiples,
//! sizing, tick-rounding, in-range + min-R checks). On any
//! [`ResolveError`](trade_control_core::intent::ResolveError) it logs loudly and
//! **declines the bar** (`PlacementReport::Declined`) without stamping the enter
//! done, so a transient failure (ATR warmup) recovers on a later bar — v1's
//! decline-this-bar-stay-armed behaviour.
//!
//! Still deferred to a later slice: adapting the **live**
//! [`Broker`](trade_control_core::broker::Broker) onto [`EntryBroker`] (a
//! [`PlacedOrder`] → [`EntryRequest`](trade_control_core::broker::EntryRequest) map,
//! now with nothing left to resolve) and `ClosePosition` (the news-reversal-close
//! slice), both on this established path.
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

use trade_control_core::intent::{
    Direction, Intent, ResolveError, Resolved, ResolvedEntry, RiskBudget,
};
use trade_control_core::plan_eval::FiredIntent;

use crate::effect::Effect;
use crate::facts::{EntryOutcome, FactKind, FactValue, Facts};
use crate::late_entry::{self, LateEntry, LateEntryOrder};
use crate::shell::shell_from_candle;
use crate::{Candle, EntryMechanism, TradePlan, tick_once};

/// A **fully resolved** entry order, ready to place. The v2-native, broker-agnostic
/// shape the [`EntryBroker`] receives — carrying the concrete trigger/SL/TP/risk the
/// executor resolved from the intent via [`Resolved::from_intent`]. It mirrors the
/// live [`EntryRequest`](trade_control_core::broker::EntryRequest) minus the
/// broker-side fields (dry-run, caps): the deferred live-broker adaptation is a plain
/// `PlacedOrder` → `EntryRequest` map with nothing left to resolve.
// No `PartialEq`: [`RiskBudget`] carries `f64` and derives none, so a resolved
// order isn't equatable. Tests assert on the individual fields instead.
#[derive(Debug, Clone)]
pub struct PlacedOrder {
    /// The instrument to trade (OANDA `EUR_USD` / TradeNation `EUR/USD`).
    pub instrument: String,
    /// Trade direction.
    pub direction: Direction,
    /// How the order rests (stop / limit / market).
    pub mechanism: EntryMechanism,
    /// The resolved resting trigger price. `None` for a **market** order (no
    /// resting trigger — it fills at the current price); `Some` for stop/limit,
    /// resolved from the intent's `EntrySpec` against the firing bar's [`Shell`].
    pub trigger: Option<f64>,
    /// Resolved stop-loss price (absolute, tick-snapped).
    pub stop_loss: f64,
    /// Resolved take-profit price (absolute, tick-snapped).
    pub take_profit: f64,
    /// How much to commit — percent of equity, a fixed money amount, or explicit
    /// units. Resolved from the intent's `risk_pct` / `risk_amount` / `size_units`.
    pub risk: RiskBudget,
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
    /// Entry **resolution** failed (geometry not ready, ATR warmup, below-min-R,
    /// out-of-range, …) — the intent could not be turned into concrete prices this
    /// bar. Logged loudly (`error!`) with full context, then the enter is **not**
    /// stamped done so it re-ticks and retries next bar. Distinct from
    /// [`Rejected`](Self::Rejected) (the broker said no to a *resolved* order):
    /// this is "couldn't even build the order yet". Matches v1's
    /// `pine_entry_dispatchable`, which treats every `from_intent` `Err` as
    /// decline-this-bar-stay-armed (so a transient warmup recovers on a later bar).
    Declined { rule_id: String, reason: String },
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
                fired, mechanism, ..
            } = effect
            {
                // Entry RESOLUTION — turn the intent + firing bar into concrete
                // trigger/SL/TP/risk via v1's proven resolver. On failure, log
                // loudly and DECLINE (don't stamp done): the enter re-ticks next bar
                // and recovers if it was transient (ATR warmup). See `resolve_order`.
                let order = match resolve_order(plan, &fired, mechanism) {
                    Ok(order) => order,
                    Err(err) => {
                        let candle = &fired.candle;
                        tracing::error!(
                            rule_id = %fired.rule_id,
                            instrument = %fired.intent.instrument,
                            error = %err,
                            error_debug = ?err,
                            bar_time = %candle.time,
                            bar_ohlc = ?(candle.o, candle.h, candle.l, candle.c),
                            has_signal = fired.signal.is_some(),
                            "entry resolution failed — declining this bar, staying armed (will retry next bar)"
                        );
                        placements.push(PlacementReport::Declined {
                            rule_id: fired.rule_id.clone(),
                            reason: err.to_string(),
                        });
                        continue;
                    }
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

/// Resolve a fired enter intent into a concrete [`PlacedOrder`] — the entry
/// *resolution* step. Reuses v1's [`Resolved::from_intent`] verbatim (anchor /
/// offset / R-multiple / sizing / tick-rounding / in-range + min-R checks), bridged
/// only by [`shell_from_candle`] which projects the v2 firing bar + latched signal
/// onto the [`Shell`](trade_control_core::intent::Shell) the resolver consumes.
///
/// `pip_size` / `tick_size` come off the **intent** (baked by tv-arm from
/// `instrument-lookup` — see the `pip_size_baked_into_intent` note), so the executor
/// never consults a catalog. Fallbacks match the worker's: pip → `0.0001`, tick →
/// `0.0` (identity, no rounding). The direction is the intent's when set, else the
/// plan's (control intents carry none).
///
/// On any [`ResolveError`] the caller logs it loudly and declines the bar — the
/// enter stays armed and retries, so a transient warmup (`AtrUnavailable`) recovers.
fn resolve_order(
    plan: &TradePlan,
    fired: &FiredIntent,
    mechanism: EntryMechanism,
) -> Result<PlacedOrder, ResolveError> {
    let intent: &Intent = &fired.intent;
    let shell = shell_from_candle(&fired.candle, fired.signal.as_ref());
    let pip_size = intent.pip_size.unwrap_or(0.0001);
    let tick_size = intent.tick_size.unwrap_or(0.0);

    let resolved = Resolved::from_intent(intent, &shell, pip_size, tick_size)?;

    // A market order has no resting trigger; a stop/limit rests at its trigger.
    let trigger = match resolved.entry {
        ResolvedEntry::Market { .. } => None,
        ResolvedEntry::Stop { trigger_price } | ResolvedEntry::Limit { trigger_price } => {
            Some(trigger_price)
        }
    };

    Ok(PlacedOrder {
        instrument: resolved.instrument,
        direction: intent.direction.unwrap_or(plan.direction),
        mechanism,
        trigger,
        stop_loss: resolved.stop_loss,
        take_profit: resolved.take_profit,
        risk: resolved.risk,
    })
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

    /// A candle with a real range around `close` (±20 pips) so a `Low`-anchored SL
    /// resolves to a non-degenerate stop distance — otherwise `Resolved::from_intent`
    /// would reject the entry with zero R. `o == c` (flat body); `h`/`l` span it.
    fn candle(time: &str, close: f64) -> Candle {
        Candle {
            time: ts(time),
            o: close,
            h: close + 0.0020,
            l: close - 0.0020,
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
            // A resolvable long-market spec: fill at close, SL at the bar low − 2
            // pips, TP at 2R. With the ±20-pip `candle()` range this resolves to a
            // real trigger/SL/TP so the executor's resolution step succeeds.
            entry: Some(trade_control_core::intent::EntrySpec::Market),
            stop_loss: Some(trade_control_core::intent::PriceRef::Anchored {
                from: trade_control_core::intent::PriceAnchor::Low,
                offset_pips: -2.0,
                offset_atr_pct: None,
            }),
            take_profit: Some(trade_control_core::intent::TakeProfit::RMultiple {
                from: trade_control_core::intent::PriceAnchor::Close,
                offset_r: 2.0,
            }),
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

    /// A resting long stop for the `place_one`-direct catch-up tests. Only
    /// `mechanism`/`direction`/`trigger` feed the late-entry parity branch these
    /// tests exercise; SL/TP/risk are inert placeholders (the parity check never
    /// reads them).
    fn stop_long(trigger: f64) -> PlacedOrder {
        PlacedOrder {
            instrument: "EUR_USD".into(),
            direction: Direction::Long,
            mechanism: EntryMechanism::Stop,
            trigger: Some(trigger),
            stop_loss: trigger - 0.0020,
            take_profit: trigger + 0.0040,
            risk: RiskBudget::Percent(1.0),
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

    // --- Entry resolution (this slice) ---------------------------------------

    /// The resolvable long-market enter resolves to concrete SL/TP the broker sees.
    /// close = 1.1000, bar low = 1.0980 (candle() spans ±20 pips), pip = 0.0001:
    ///   SL = low − 2 pips = 1.0978; R = 1.1000 − 1.0978 = 0.0022;
    ///   TP = close + 2R = 1.1044. Market entry ⇒ no resting trigger.
    #[tokio::test]
    async fn enter_resolves_concrete_sl_tp_for_the_broker() {
        let p = plan(vec![enter_rule()]);
        let mut facts = Facts::default();
        let broker = FakeBroker::ok();
        let store = FakeStore::default();

        Execution {
            broker: &broker,
            store: &store,
        }
        .drive_bar(
            &p,
            &mut facts,
            &[candle("2026-06-01T10:00:00Z", 1.1000)],
            ts("2026-06-01T10:00:00Z"),
            true,
            &[],
        )
        .await;

        let placed = broker.placed.borrow();
        let order = placed.first().expect("one order placed");
        assert!(
            order.trigger.is_none(),
            "market entry has no resting trigger"
        );
        assert!(
            (order.stop_loss - 1.0978).abs() < 1e-9,
            "SL = low − 2 pips, got {}",
            order.stop_loss
        );
        assert!(
            (order.take_profit - 1.1044).abs() < 1e-9,
            "TP = close + 2R, got {}",
            order.take_profit
        );
        assert!(
            matches!(order.risk, RiskBudget::Percent(p) if (p - 1.0).abs() < 1e-9),
            "risk resolves to the intent's 1% default, got {:?}",
            order.risk
        );
    }

    /// A resolve FAILURE (here: an intent missing its `stop_loss`) declines the bar
    /// and does NOT stamp the enter done — so it re-ticks and retries next bar. This
    /// is the key design decision: a resolution failure (geometry not ready) is
    /// distinct from a caught-up miss and must never permanently retire a setup.
    #[tokio::test]
    async fn resolve_failure_declines_and_stays_armed() {
        let mut broken = enter_rule();
        broken.intent.stop_loss = None; // → ResolveError::MissingField("stop_loss")
        let p = plan(vec![broken]);
        let mut facts = Facts::default();
        let broker = FakeBroker::ok();
        let store = FakeStore::default();

        let exec = Execution {
            broker: &broker,
            store: &store,
        };

        let r = exec
            .drive_bar(
                &p,
                &mut facts,
                &[candle("2026-06-01T10:00:00Z", 1.1000)],
                ts("2026-06-01T10:00:00Z"),
                true,
                &[],
            )
            .await;

        assert!(
            matches!(
                r.placements.as_slice(),
                [PlacementReport::Declined { rule_id, .. }] if rule_id == "05-enter"
            ),
            "resolve failure → Declined, got {:?}",
            r.placements
        );
        assert!(broker.placed.borrow().is_empty(), "nothing placed");
        assert!(
            store.stamps.borrow().is_empty(),
            "NOT stamped done — the enter stays armed",
        );
        assert!(
            !facts.is_set_named("05-enter", EntryOutcome::NAME),
            "no fire-once fact — the enter re-ticks and retries next bar",
        );

        // Prove it retries: fix the geometry, tick again → it places this time.
        let mut fixed = enter_rule();
        fixed.intent.pip_size = None;
        let p2 = plan(vec![fixed]);
        let r2 = exec
            .drive_bar(
                &p2,
                &mut facts,
                &[candle("2026-06-01T11:00:00Z", 1.1000)],
                ts("2026-06-01T11:00:00Z"),
                true,
                &[],
            )
            .await;
        assert!(
            matches!(r2.placements.as_slice(), [PlacementReport::Placed { .. }]),
            "with geometry resolvable it places, got {:?}",
            r2.placements
        );
    }
}
