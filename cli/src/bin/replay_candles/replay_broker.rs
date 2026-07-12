//! A fake [`Broker`] for offline multi-shot replay.
//!
//! The shared multi-shot gate (`trade_control_core::retry_gate::evaluate`) is
//! async and asks the **broker** whether a prior attempt is still open before
//! allowing a re-entry. Live, that broker is TradeNation/OANDA. Offline, this
//! `ReplayBroker` approximates the answer from candles: each placed attempt is
//! re-simulated with [`simulate_fill`] **up to the bar the gate is asking on**
//! (time-accurate — a re-entry only clears once the prior attempt has really
//! closed by that bar), and the [`SimOutcome`] is mapped to an [`AttemptState`].
//!
//! Only the retry-gate-relevant methods do real work
//! (`lookup_attempt_state`, `list_open_positions`, `cancel_order`); the replay
//! never places real orders, so `place_entry` and the rest are stubs.

use std::cell::RefCell;

use chrono::{DateTime, Utc};
use trade_control_core::broker::{
    AmendError, AttemptState, BidAskCandle, Broker, CancelError, Candle, CandleError, EntryError,
    EntryRequest, Granularity, LookupError, OpenPosition, PendingOrder, Quote,
};
use trade_control_core::intent::{Direction, Intent, Resolved, Shell};
use trade_control_engine::{
    BidAskCandle as EngineCandle, SimOutcome, apply_entry_spread_floor, simulate_fill,
    simulate_fill_windowed,
};

use super::report::{CloseFire, FillKind};

/// One placed attempt the gate may later ask about, with the geometry needed to
/// re-simulate it. `order_id` is what [`Broker::place_entry`] handed back (the
/// retry gate keys on it); `shell` + `intent` resolve the entry/SL/TP.
#[derive(Clone)]
struct PlacedAttempt {
    order_id: String,
    intent: Intent,
    shell: Shell,
    /// Set once the gate cancels this resting order (supersede path). A
    /// cancelled attempt resolves to [`AttemptState::Cancelled`] regardless of
    /// the price path.
    cancelled: bool,
    /// The position-ledger geometry (PR 4b-1): the forward candle path and the
    /// entry-spread statistic this order's *realized* outcome is simulated
    /// against. Present only when the attempt was recorded through
    /// [`ReplayBroker::record_order`] (the ledger path); the plain
    /// retry-gate `record_attempt` leaves it `None` and only the `as_of`-bounded
    /// [`ReplayBroker::resolve`] answers apply. Kept separate so the retry-gate
    /// state answers (`lookup_attempt_state` etc., which bound at `as_of`) are
    /// untouched by the ledger, which walks the FULL forward path like the report.
    //
    // Dead outside `cfg(test)` until PR 4b-2 wires `record_order` /
    // `realized_outcome` into the replay loop + report; the shadow-parity tests
    // are the ledger's only consumer in 4b-1. Every `allow(dead_code)` below is
    // removed the moment 4b-2 switches the report over to broker state.
    #[allow(dead_code)]
    ledger: Option<LedgerGeometry>,
}

/// The per-order geometry the position ledger (PR 4b-1) advances a realized
/// outcome against — the SAME inputs `report.rs::resolve_fire_any` walks: the
/// full forward bid/ask path from the fire bar, the trailing entry-spread mean
/// (for the SL floor), and the reversal-close fires that could flatten the
/// position early. The ledger reuses the engine's fill physics
/// (`simulate_fill_windowed`, `apply_entry_spread_floor`, and the reversal-close
/// post-pass) so its outcome reproduces the report's bit-for-bit.
#[derive(Clone)]
#[allow(dead_code)] // 4b-1 scaffold; consumed by tests, wired into the loop in 4b-2.
struct LedgerGeometry {
    /// Bid/ask candles at/after the fire bar (ascending) — the fill sim input
    /// and the `until` window-end anchor (`forward.last()`).
    forward: Vec<EngineCandle>,
    /// The trailing-window mean entry spread (through the shared
    /// `get_bidask_candles` provider) the SL floor sizes off. `None` ⇒ fall back
    /// to the fire bar's own close spread (mirrors the report).
    entry_spread_price: Option<f64>,
    /// The `06-close-on-reversal` fires the position could exit on before its
    /// SL/TP (the reversal-close post-pass). Empty ⇒ no reversal-close applies.
    closes: Vec<CloseFire>,
}

/// A reversal-close that flattened an open ledger position before its SL/TP —
/// the [`apply_reversal_close`] verdict, carrying the fill it applies to and the
/// close bar/price. Mirrors `report.rs`'s private `ReplayOutcome::ClosedOnReversal`.
#[allow(dead_code)] // 4b-1 scaffold; consumed by tests, wired into the loop in 4b-2.
struct ReversalClose {
    fill_at: DateTime<Utc>,
    entry_price: f64,
    exit_at: DateTime<Utc>,
    exit_price: f64,
}

/// Whether a `06-close-on-reversal` fire flattens this outcome's open position
/// before its own SL/TP — a **verbatim** lift of `report.rs::apply_reversal_close`
/// so the ledger's reversal handling matches the report bit-for-bit. A close bar
/// after the fill (and strictly before any SL/TP exit) closes the position; the
/// earliest such close wins. `None` ⇒ no reversal applies (untaken outcomes, or a
/// close that lands outside the open window).
#[allow(dead_code)] // 4b-1 scaffold; consumed by tests, wired into the loop in 4b-2.
fn apply_reversal_close(outcome: &SimOutcome, closes: &[CloseFire]) -> Option<ReversalClose> {
    let (fill_at, entry_price, exit_limit) = match outcome {
        SimOutcome::FilledOpen {
            fill_at,
            entry_price,
        } => (*fill_at, *entry_price, None),
        SimOutcome::StoppedOut {
            fill_at,
            entry_price,
            exit_at,
            ..
        }
        | SimOutcome::TookProfit {
            fill_at,
            entry_price,
            exit_at,
            ..
        } => (*fill_at, *entry_price, Some(*exit_at)),
        // No open position to close.
        SimOutcome::NeverFilled
        | SimOutcome::Declined { .. }
        | SimOutcome::SpreadBlackout { .. }
        | SimOutcome::Unresolved(_) => return None,
    };
    closes
        .iter()
        .filter(|c| c.at > fill_at)
        .filter(|c| match exit_limit {
            // Only a reversal strictly before the SL/TP bar pre-empts it.
            Some(exit_at) => c.at < exit_at,
            None => true,
        })
        .min_by_key(|c| c.at)
        .map(|c| ReversalClose {
            fill_at,
            entry_price,
            exit_at: c.at,
            exit_price: c.price,
        })
}

/// A placed order's *realized* outcome, driven from the position ledger — the
/// broker-owned equivalent of the report's `FireResult` (PR 4b-1 shadow). Carries
/// the same load-bearing fields `resolve_fire_any` produces so the report can
/// later (4b-2) read this instead of re-simulating: direction, the fill bar +
/// price, the box's right edge, the (floored) SL/TP, and the taken/closed kind.
///
/// A cancelled order has no realized outcome — `realized_outcome` returns `None`
/// for it, which is the whole point of the ledger (a spread-hour cancel later
/// flows into a "no fill" here).
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // 4b-1 scaffold; consumed by tests, wired into the loop in 4b-2.
pub struct RealizedOutcome {
    pub direction: Direction,
    /// Open-time of the bar the entry filled on (or the fire bar, for a
    /// not-taken kind — mirrors `FireResult`).
    pub fill_at: DateTime<Utc>,
    /// Right-edge time anchor: the exit bar for a closed trade, else the last
    /// forward bar (open at window end / not taken).
    pub until: DateTime<Utc>,
    /// The level the fill happened at (or the intended placed level, not taken).
    pub entry_price: f64,
    /// The floored stop the position rested on.
    pub stop_loss: f64,
    pub take_profit: f64,
    pub kind: FillKind,
}

/// The geometry the replay loop arms before each `run_enter` so this broker's
/// `place_entry` can mint a correlatable order id and record the attempt. The
/// real dispatch (`run_enter`) calls `broker.place_entry` with only an
/// `EntryRequest`, which lacks the intent + shell the offline prior-attempt
/// resolver needs — so the loop hands them in out-of-band here.
#[derive(Clone)]
struct ArmedPlacement {
    order_id: String,
    intent: Intent,
    shell: Shell,
}

/// Offline broker that resolves prior-attempt state from the candle window.
pub struct ReplayBroker {
    /// The full pulled bid/ask candle window (warm-up + live), ascending. Each
    /// lookup re-simulates an attempt against the prefix up to the asking bar,
    /// filling each leg on the real book side.
    candles: Vec<BidAskCandle>,
    pip_size: f64,
    /// The bar the gate is currently asking about — its open time. Set by the
    /// replay loop before each `evaluate`, so `lookup_attempt_state` bounds its
    /// simulation at this bar (time-accurate prior-state resolution).
    as_of: RefCell<DateTime<Utc>>,
    placed: RefCell<Vec<PlacedAttempt>>,
    /// The placement the loop armed for the next `run_enter` (its intent, shell,
    /// and the order id `place_entry` should return). Consumed by `place_entry`.
    armed: RefCell<Option<ArmedPlacement>>,
}

impl ReplayBroker {
    pub fn new(candles: Vec<BidAskCandle>, pip_size: f64) -> Self {
        let last = candles.last().map(|c| c.time).unwrap_or_else(Utc::now);
        Self {
            candles,
            pip_size,
            as_of: RefCell::new(last),
            placed: RefCell::new(Vec::new()),
            armed: RefCell::new(None),
        }
    }

    /// Point all subsequent prior-attempt lookups at `as_of` (the open time of
    /// the bar the gate is evaluating). Call before each `retry_gate::evaluate`.
    pub fn set_as_of(&self, as_of: DateTime<Utc>) {
        *self.as_of.borrow_mut() = as_of;
    }

    /// Arm the placement for the next `run_enter`: the order id `place_entry`
    /// should return and the intent + shell needed to resolve this attempt's
    /// later state. Call right before dispatching the enter; `place_entry`
    /// consumes it. `order_id` must match what the gate stores on the
    /// `EntryAttempt` (`run_enter` stamps `place_entry`'s return there), so the
    /// minted id is the standard `{intent.id}-{attempt_no}` form.
    pub fn arm_placement(&self, order_id: String, intent: Intent, shell: Shell) {
        *self.armed.borrow_mut() = Some(ArmedPlacement {
            order_id,
            intent,
            shell,
        });
    }

    /// Register a placed attempt so a later lookup can resolve it. `order_id`
    /// must match what the gate stored on the `EntryAttempt` (the replay uses
    /// the same id when it `record_placement`s).
    fn record_attempt(&self, order_id: String, intent: Intent, shell: Shell) {
        self.placed.borrow_mut().push(PlacedAttempt {
            order_id,
            intent,
            shell,
            cancelled: false,
            ledger: None,
        });
    }

    /// Record a placed order **with its position-ledger geometry** (PR 4b-1) so a
    /// later [`ReplayBroker::realized_outcome`] can advance it to a realized
    /// outcome. `forward` is the fire bar onward, `entry_spread_price` the
    /// trailing-window mean the SL floor sizes off, and `closes` the
    /// reversal-close fires that could flatten it early — the SAME three inputs
    /// `report.rs::resolve_fire_any` walks. Retry-gate state answers are
    /// unaffected: `resolve` (bounded at `as_of`) still drives them from the raw
    /// intent + shell.
    #[allow(dead_code)] // 4b-1 scaffold; consumed by tests, wired into the loop in 4b-2.
    pub fn record_order(
        &self,
        order_id: String,
        intent: Intent,
        shell: Shell,
        forward: Vec<EngineCandle>,
        entry_spread_price: Option<f64>,
        closes: Vec<CloseFire>,
    ) {
        self.placed.borrow_mut().push(PlacedAttempt {
            order_id,
            intent,
            shell,
            cancelled: false,
            ledger: Some(LedgerGeometry {
                forward,
                entry_spread_price,
                closes,
            }),
        });
    }

    /// The realized outcome of a ledger-tracked order (PR 4b-1) — the
    /// broker-owned equivalent of `report.rs::resolve_fire_any`'s taken/closed
    /// verdict for the same enter. Advances the order's forward path through the
    /// SAME engine physics the report uses, in the SAME per-bar precedence:
    ///
    ///   fill → strategy-side/simulator SL/TP → reversal-close → break-even
    ///
    /// (break-even is folded into `simulate_fill_windowed`, and the SL floor into
    /// `apply_entry_spread_floor` — so this driver just calls them in report order).
    ///
    /// Returns `None` when the order was **cancelled** (no fill, no outcome — the
    /// lifecycle-cancel case later stages exercise) or wasn't recorded with ledger
    /// geometry (a plain retry-gate attempt).
    #[allow(dead_code)] // 4b-1 scaffold; consumed by tests, wired into the loop in 4b-2.
    pub fn realized_outcome(&self, order_id: &str) -> Option<RealizedOutcome> {
        let placed = self.placed.borrow();
        let attempt = placed.iter().find(|a| a.order_id == order_id)?;
        // A cancelled order never fills — no realized outcome (the whole point of
        // the ledger: a spread-hour cancel flows into "no fill" here).
        if attempt.cancelled {
            return None;
        }
        let geo = attempt.ledger.as_ref()?;
        Some(self.realize(&attempt.intent, &attempt.shell, geo))
    }

    /// Compute a ledger order's realized outcome by replaying the report's exact
    /// sequence — `apply_entry_spread_floor` (floors the DISPLAYED SL/TP) then
    /// `simulate_fill_windowed` (the fill/exit, with the same floor + break-even
    /// baked in) then the reversal-close post-pass — and map to a
    /// [`RealizedOutcome`]. This is a verbatim lift of `resolve_fire_any`'s taken
    /// path, so the two agree bit-for-bit (the 4b-1 shadow-parity gate).
    #[allow(dead_code)] // 4b-1 scaffold; consumed by tests, wired into the loop in 4b-2.
    fn realize(&self, intent: &Intent, shell: &Shell, geo: &LedgerGeometry) -> RealizedOutcome {
        let pip_size = self.pip_size;
        let tick = intent.tick_size.unwrap_or(pip_size);
        // The floored bracket the position rests on: resolve, then apply the
        // entry-spread floor in place (mirrors `resolve_fire_any`, which floors
        // `resolved` before reading its SL/TP). A resolution failure can't happen
        // for a ledger order the report would have drawn (it resolved to place the
        // order), so fall back to the raw resolve and let the sim's `Unresolved`
        // path drive the outcome.
        let mut resolved = Resolved::from_intent(intent, shell, pip_size, tick).ok();
        if let Some(r) = resolved.as_mut() {
            apply_entry_spread_floor(r, pip_size, &geo.forward, geo.entry_spread_price);
        }
        let direction = resolved
            .as_ref()
            .map(|r| r.direction)
            .unwrap_or(Direction::Long);
        let stop_loss = resolved.as_ref().map(|r| r.stop_loss).unwrap_or(0.0);
        let take_profit = resolved.as_ref().map(|r| r.take_profit).unwrap_or(0.0);
        let placed_level = resolved
            .as_ref()
            .map(|r| r.entry.reference_price())
            .unwrap_or(0.0);
        // The not-taken / open box runs to the last forward bar; a closed trade
        // overrides `until` with its exit bar below.
        let window_end = geo.forward.last().map(|c| c.time).unwrap_or(shell.time);
        let fire_at = shell.time;

        let raw = simulate_fill_windowed(
            intent,
            shell,
            pip_size,
            &geo.forward,
            geo.entry_spread_price,
        );
        let (fill_at, until, entry_price, kind) = match apply_reversal_close(&raw, &geo.closes) {
            Some(rc) => (
                rc.fill_at,
                rc.exit_at,
                rc.entry_price,
                FillKind::ClosedOnReversal,
            ),
            None => match &raw {
                SimOutcome::FilledOpen {
                    fill_at,
                    entry_price,
                } => (*fill_at, window_end, *entry_price, FillKind::Open),
                SimOutcome::StoppedOut {
                    fill_at,
                    entry_price,
                    exit_at,
                    ..
                } => (*fill_at, *exit_at, *entry_price, FillKind::StoppedOut),
                SimOutcome::TookProfit {
                    fill_at,
                    entry_price,
                    exit_at,
                    ..
                } => (*fill_at, *exit_at, *entry_price, FillKind::TookProfit),
                SimOutcome::NeverFilled => {
                    (fire_at, window_end, placed_level, FillKind::NeverFilled)
                }
                SimOutcome::Declined { .. } => {
                    (fire_at, window_end, placed_level, FillKind::Declined)
                }
                SimOutcome::SpreadBlackout { .. } => {
                    (fire_at, window_end, placed_level, FillKind::SpreadBlackout)
                }
                // `Unresolved` has nothing to draw; the report returns `None`. The
                // ledger surfaces a not-taken Declined so the API stays total —
                // callers filter on `kind.is_taken()` exactly as the report does.
                SimOutcome::Unresolved(_) => {
                    (fire_at, window_end, placed_level, FillKind::Declined)
                }
            },
        };
        RealizedOutcome {
            direction,
            fill_at,
            until,
            entry_price,
            stop_loss,
            take_profit,
            kind,
        }
    }

    /// The order ids the gate has cancelled so far (the cancel-and-replace
    /// path — a later sibling/re-entry superseded a still-resting order). The
    /// replay loop reads this after each gate call to stamp the superseded
    /// `Fire` so the report shows it as cancelled, not a fabricated fill.
    pub fn cancelled_order_ids(&self) -> Vec<String> {
        self.placed
            .borrow()
            .iter()
            .filter(|a| a.cancelled)
            .map(|a| a.order_id.clone())
            .collect()
    }

    /// Candles up to and including the `as_of` bar — the slice a prior attempt
    /// is simulated against. Bounding here is what makes re-entry time-accurate.
    fn window_to_as_of(&self) -> Vec<BidAskCandle> {
        let as_of = *self.as_of.borrow();
        self.candles
            .iter()
            .filter(|c| c.time <= as_of)
            .cloned()
            .collect()
    }

    /// The bid/ask candle at the current `as_of` bar (the bar `run_enter` is
    /// firing on, since the replay loop calls `set_as_of(fire_bar.time)` right
    /// before dispatching). This is the closed fire bar whose book the live
    /// worker would sample with a `get_quote` round-trip. Falls back to the last
    /// candle at/before `as_of` if the exact open time isn't present (it always
    /// is in the replay's closed loop, but stay robust).
    fn candle_at_as_of(&self) -> Option<&BidAskCandle> {
        let as_of = *self.as_of.borrow();
        self.candles.iter().rfind(|c| c.time <= as_of)
    }

    /// Resolve a placed attempt's current state from its price path up to
    /// `as_of`. The attempt's own candles are those at/after its shell time
    /// (the bar it fired on) within the bounded window.
    fn resolve(&self, attempt: &PlacedAttempt) -> AttemptState {
        if attempt.cancelled {
            return AttemptState::Cancelled;
        }
        let window = self.window_to_as_of();
        // Forward path = candles from the firing bar onward (simulate_fill walks
        // these to find the fill, then the SL/TP touch).
        let forward: Vec<BidAskCandle> = window
            .into_iter()
            .filter(|c| c.time >= attempt.shell.time)
            .collect();
        match simulate_fill(&attempt.intent, &attempt.shell, self.pip_size, &forward) {
            SimOutcome::StoppedOut { .. } => {
                AttemptState::ClosedLossOrBreakeven { realized_pl: -1.0 }
            }
            SimOutcome::TookProfit { .. } => AttemptState::ClosedWin { realized_pl: 1.0 },
            SimOutcome::FilledOpen { .. } => AttemptState::OpenPosition {
                broker_trade_id: format!("{}-pos", attempt.order_id),
            },
            // Not filled by the asking bar = a still-**resting** order, exactly
            // what the real broker reports as `Pending`. This is load-bearing
            // for strategy-v2: a sibling enter (QM limit vs break-and-close stop)
            // firing on a later bar must see the prior resting order as `Pending`
            // so the gate **cancels and replaces** it (cancel-and-replace), and
            // so a still-resting order can't go on to fill alongside the new one.
            // Returning `Cancelled` here (the old behaviour) silently let both
            // orders rest+fill → overlapping positions (Bug 1 + Bug 2). A
            // genuinely cancelled order is caught above by `attempt.cancelled`.
            // (`expiry_bars`-driven expiry is folded into `simulate_fill`'s fill
            // window, so an expired order resolves to `NeverFilled`/`Pending`
            // here too — these v2 plans don't set `expiry_bars`, and the gate's
            // cap/window bound the re-entry count regardless.)
            SimOutcome::NeverFilled => AttemptState::Pending,
            // Declined / spread-blackout / unresolved — no order ever went on;
            // the slot is free.
            SimOutcome::Declined { .. }
            | SimOutcome::SpreadBlackout { .. }
            | SimOutcome::Unresolved(_) => AttemptState::Cancelled,
        }
    }

    /// Build the [`PendingOrder`] a live broker would report for a still-resting
    /// attempt. `trigger`/`is_stop` come from resolving the intent's entry
    /// against its shell; a Market entry (which never rests) or a resolution
    /// failure falls back to the shell close as the trigger and `is_stop=true`.
    /// The lifecycle's cancel decision keys off `instrument` + `order_id`, so
    /// `trigger`/`stake` are informational — but resolve them accurately when we
    /// can so a report renders the right level.
    fn pending_from_attempt(&self, a: &PlacedAttempt) -> PendingOrder {
        use trade_control_core::intent::{Direction, Resolved, ResolvedEntry};
        let direction = a.intent.direction.unwrap_or(Direction::Long);
        let (trigger, is_stop) =
            match Resolved::from_intent(&a.intent, &a.shell, self.pip_size, self.pip_size) {
                Ok(r) => match r.entry {
                    ResolvedEntry::Stop { trigger_price } => (trigger_price, true),
                    ResolvedEntry::Limit { trigger_price } => (trigger_price, false),
                    ResolvedEntry::Market { reference_price } => (reference_price, true),
                },
                Err(_) => (a.shell.close, true),
            };
        PendingOrder {
            order_id: a.order_id.clone(),
            instrument: a.intent.instrument.clone(),
            direction,
            trigger,
            is_stop,
            stake: 1.0,
        }
    }
}

impl Broker for ReplayBroker {
    async fn place_entry(
        &self,
        _max_risk_pct: f64,
        _max_open_positions: u32,
        _req: &EntryRequest<'_>,
    ) -> Result<String, EntryError> {
        // The real dispatch (`run_enter`) calls this to "place" the order. The
        // replay loop armed the geometry out-of-band (intent + shell + the order
        // id to return) because `EntryRequest` lacks what the offline
        // prior-attempt resolver needs. Record the attempt so a later
        // `lookup_attempt_state` can resolve it, and hand back the armed id —
        // which `run_enter` then stamps onto the `EntryAttempt` row, keeping the
        // gate's correlation intact.
        let armed = self.armed.borrow_mut().take();
        match armed {
            Some(a) => {
                self.record_attempt(a.order_id.clone(), a.intent, a.shell);
                Ok(a.order_id)
            }
            // No armed placement means the loop dispatched an enter without
            // arming first — a wiring bug, not a broker condition. Fail the
            // placement loudly rather than fabricate an id.
            None => {
                tracing::error!(
                    "ReplayBroker::place_entry called with no armed placement — replay wiring bug"
                );
                Err(EntryError::OrderRejected)
            }
        }
    }

    async fn close_positions(&self, _instrument: &str) -> bool {
        false
    }

    async fn cancel_pending_for_instrument(&self, _instrument: &str) -> usize {
        0
    }

    async fn lookup_attempt_state(
        &self,
        _instrument: &str,
        broker_order_id: &str,
        _broker_trade_id: Option<&str>,
    ) -> Result<AttemptState, LookupError> {
        let placed = self.placed.borrow();
        match placed.iter().find(|a| a.order_id == broker_order_id) {
            Some(a) => Ok(self.resolve(a)),
            // The gate only asks about ids we placed; an unknown id means the
            // attempt was never recorded — treat as Unknown (fail-safe in the
            // gate, though this shouldn't happen in the replay's closed loop).
            None => Ok(AttemptState::Unknown),
        }
    }

    async fn cancel_order(
        &self,
        _account_id: &str,
        broker_order_id: &str,
    ) -> Result<(), CancelError> {
        if let Some(a) = self
            .placed
            .borrow_mut()
            .iter_mut()
            .find(|a| a.order_id == broker_order_id)
        {
            a.cancelled = true;
        }
        Ok(())
    }

    async fn get_quote(&self, _instrument: &str) -> Result<Quote, LookupError> {
        // The shared entry gates (spread-blackout + SL-vs-spread floor in
        // `dispatch::run_enter`) sample the live spread via this round-trip. The
        // replay candles carry the real book (`bid_c`/`ask_c`), so synthesize the
        // quote from the fire bar's close rather than failing open: that lets the
        // offline replay REPRODUCE a spread rejection the live worker would make,
        // tightening replay↔live parity.
        //
        // Fidelity caveat: a closed bar's `bid_c`/`ask_c` is the spread *at the
        // bar's close*, a coarse proxy for the live worker's instant-of-fire
        // sample. It captures sustained-wide spreads — exactly the post-NY-close
        // liquidity trough the spread-blackout window targets — but not a brief
        // intrabar spike that retraces by the close. So the replay reproduces the
        // common case (sustained wide) and under-reports the sub-bar-spike edge.
        // Better than the old unconditional fail-open, which reproduced nothing.
        match self.candle_at_as_of() {
            Some(c) => Ok(Quote {
                bid: c.bid_c,
                ask: c.ask_c,
            }),
            // No candle at/before `as_of` — should never happen in the replay's
            // closed loop (the fire bar is always present), but if it does, fail
            // open the same way the live worker does on a transient quote error.
            None => Err(LookupError::Transient),
        }
    }

    async fn list_open_positions(
        &self,
        _account_id: &str,
    ) -> Result<Vec<OpenPosition>, LookupError> {
        // The Bug #11 backstop: report a synthetic open position for any placed
        // attempt that resolves to OpenPosition by the asking bar, keyed back to
        // its order id so the gate's correlation matches.
        let placed = self.placed.borrow();
        let positions = placed
            .iter()
            .filter_map(|a| match self.resolve(a) {
                AttemptState::OpenPosition { broker_trade_id } => Some(OpenPosition {
                    instrument: a.intent.instrument.clone(),
                    direction: a
                        .intent
                        .direction
                        .unwrap_or(trade_control_core::intent::Direction::Long),
                    stop_loss: None,
                    take_profit: None,
                    position_id: broker_trade_id,
                    order_id: a.order_id.clone(),
                    stake: 1.0,
                }),
                _ => None,
            })
            .collect();
        Ok(positions)
    }

    async fn amend_stop(
        &self,
        _account_id: &str,
        _position_or_order_id: &str,
        _new_stop: f64,
    ) -> Result<(), AmendError> {
        Ok(())
    }

    async fn list_pending_orders(
        &self,
        _account_id: &str,
    ) -> Result<Vec<PendingOrder>, LookupError> {
        // Report a synthetic resting order for every placed attempt that
        // `resolve`s to `Pending` at `as_of` — i.e. an order that has been
        // "placed" but not yet filled/cancelled by the asking bar. This is what
        // the shared `pending_order_lifecycle` (core) lists to decide what to
        // cancel through a spread hour; a mock that always returned `[]` (the
        // pre-PR-3 stub) would make the lifecycle a no-op offline, so replay
        // could never reproduce the live cancel/restore. Mirrors
        // `list_open_positions`' reconstruction from `placed`.
        let placed = self.placed.borrow();
        let pendings = placed
            .iter()
            .filter(|a| matches!(self.resolve(a), AttemptState::Pending))
            .map(|a| self.pending_from_attempt(a))
            .collect();
        Ok(pendings)
    }

    async fn get_candles(
        &self,
        _instrument: &str,
        _granularity: Granularity,
        _since: DateTime<Utc>,
        _now: DateTime<Utc>,
    ) -> Result<Vec<Candle>, CandleError> {
        // The replay feeds MID candles directly; the gate never fetches them.
        Ok(Vec::new())
    }

    async fn get_bidask_candles(
        &self,
        _instrument: &str,
        _granularity: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<BidAskCandle>, CandleError> {
        // THE shared bar feeder for the entry SL-spread floor: `run_enter`'s
        // `windowed_entry_spread` calls this to average the last N bars' spread
        // — the SAME code path the live worker drives through its real broker.
        // The replay serves it from its own recorded series, so worker and
        // replay size the floor off an identical statistic (no hand-sliced
        // window, no duplicated floor logic → no drift).
        //
        // Bound the window to `(since, now]`, clamped at the `as_of` bar so a
        // fire never sees candles after the bar it fired on (time-accurate,
        // same discipline as `window_to_as_of`). Closed bars only — the replay
        // series is already all-closed.
        if since >= now {
            return Err(CandleError::BadRange);
        }
        let as_of = *self.as_of.borrow();
        let upper = now.min(as_of);
        Ok(self
            .candles
            .iter()
            .filter(|c| c.time > since && c.time <= upper)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A bid==ask==mid bar (zero spread) — the books equal the mid OHLC, so the
    /// fill tests read as plain prices while still exercising the bid/ask path.
    fn candle(epoch: i64, c: f64) -> BidAskCandle {
        let (o, h, l) = (c, c + 0.001, c - 0.001);
        BidAskCandle {
            time: Utc.timestamp_opt(epoch, 0).unwrap(),
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

    /// A minimal short stop-entry enter intent (serde-built, the pattern the
    /// other replay tests use) anchored to absolute levels so resolution needs
    /// no signal geometry: entry stop at 1.1000, SL 1.1020, TP 1.0950.
    fn short_enter_intent() -> Intent {
        serde_json::from_str(
            r#"{
                "v": 1,
                "id": "t-enter",
                "not_after": "2026-06-20T00:00:00Z",
                "action": "enter",
                "instrument": "EUR/USD",
                "direction": "short",
                "entry": { "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.1000 },
                "stop_loss": { "absolute": 1.1020 },
                "take_profit": { "absolute": 1.0950 },
                "broker": "tradenation",
                "trade_id": "t",
                "max_retries": 5
            }"#,
        )
        .expect("valid enter intent")
    }

    /// A bar carrying an explicit bid/ask close spread, so `get_quote` has a
    /// non-zero book to surface. Mid OHLC are left at `c` for simplicity (the
    /// quote path reads only the bid/ask closes).
    fn spread_candle(epoch: i64, bid_c: f64, ask_c: f64) -> BidAskCandle {
        let mid = (bid_c + ask_c) / 2.0;
        BidAskCandle {
            time: Utc.timestamp_opt(epoch, 0).unwrap(),
            o: mid,
            h: mid + 0.001,
            l: mid - 0.001,
            c: mid,
            bid_o: bid_c,
            bid_h: bid_c + 0.001,
            bid_l: bid_c - 0.001,
            bid_c,
            ask_o: ask_c,
            ask_h: ask_c + 0.001,
            ask_l: ask_c - 0.001,
            ask_c,
        }
    }

    #[tokio::test]
    async fn get_quote_synthesizes_the_as_of_bar_book() {
        // Two bars with different spreads; `get_quote` must reflect whichever
        // bar `as_of` points at (the fire bar the worker would sample).
        let tight = spread_candle(0, 1.10000, 1.10002); // 0.2 pip
        let wide = spread_candle(3600, 1.10000, 1.10050); // 5.0 pip (blackout-class)
        let b = ReplayBroker::new(vec![tight, wide], 0.0001);

        // As-of the tight bar → tight quote.
        b.set_as_of(Utc.timestamp_opt(0, 0).unwrap());
        let q0 = b.get_quote("EUR/USD").await.unwrap();
        assert_eq!(q0.bid, 1.10000);
        assert_eq!(q0.ask, 1.10002);
        assert!((q0.spread() / 0.0001 - 0.2).abs() < 1e-9, "0.2 pip spread");

        // As-of the wide bar → wide quote (the spread the blackout gate rejects).
        b.set_as_of(Utc.timestamp_opt(3600, 0).unwrap());
        let q1 = b.get_quote("EUR/USD").await.unwrap();
        assert_eq!(q1.bid, 1.10000);
        assert_eq!(q1.ask, 1.10050);
        assert!((q1.spread() / 0.0001 - 5.0).abs() < 1e-9, "5.0 pip spread");
    }

    #[tokio::test]
    async fn get_quote_fails_open_with_no_candle_before_as_of() {
        // `as_of` before any candle → no book to sample → transient (fail open),
        // matching the live worker's behaviour on a quote-endpoint hiccup.
        let b = ReplayBroker::new(vec![spread_candle(3600, 1.10000, 1.10002)], 0.0001);
        b.set_as_of(Utc.timestamp_opt(0, 0).unwrap());
        let err = b.get_quote("EUR/USD").await.unwrap_err();
        assert_eq!(err, LookupError::Transient);
    }

    #[tokio::test]
    async fn unknown_order_id_resolves_unknown() {
        let b = ReplayBroker::new(vec![candle(0, 1.10)], 0.0001);
        let st = b
            .lookup_attempt_state("EUR/USD", "nope", None)
            .await
            .unwrap();
        assert_eq!(st, AttemptState::Unknown);
    }

    #[tokio::test]
    async fn cancelled_order_resolves_cancelled() {
        // Candles that would fill + stop the short (so absent the cancel it'd be
        // ClosedLossOrBreakeven); the cancel must override to Cancelled.
        let candles = vec![candle(0, 1.1000), candle(3600, 1.1025)];
        let b = ReplayBroker::new(candles, 0.0001);
        let shell = Shell::from_candle(&candle(0, 1.1000).mid());
        b.record_attempt("o1".into(), short_enter_intent(), shell);
        b.cancel_order("", "o1").await.unwrap();
        let st = b.lookup_attempt_state("EUR/USD", "o1", None).await.unwrap();
        assert_eq!(st, AttemptState::Cancelled);
    }

    #[tokio::test]
    async fn open_then_closed_as_the_asof_bar_advances() {
        // The attempt fires on bar 0 (its shell bar); a resting order isn't live
        // until that bar closes, so the fill can only land on bar 1 onward (the
        // fire-bar skip in `simulate_fill`). Here the bid reaches the 1.1000
        // sell-stop on bar 1 (fill), then the SL at 1.1020 is hit on bar 2. So
        // as-of bar 0 → not filled yet, but the order is **resting** (Pending);
        // as-of bar 1 → OpenPosition; as-of bar 2 → ClosedLossOrBreakeven.
        let fire_bar = candle(0, 1.1010); // shell/fire bar — above the trigger, no fill
        let fill_bar = candle(3600, 1.1000); // bid reaches the 1.1000 sell-stop
        let sl_bar = candle(7200, 1.1021); // SL 1.1020 hit
        let candles = vec![fire_bar, fill_bar, sl_bar];
        let b = ReplayBroker::new(candles, 0.0001);
        let shell = Shell::from_candle(&fire_bar.mid());
        b.record_attempt("o1".into(), short_enter_intent(), shell);

        // As-of the fire bar: order placed but not yet filled (can't fill on its
        // own fire bar). It's a live **resting** order → Pending, exactly what the
        // real broker reports — so a sibling enter would cancel-and-replace it.
        b.set_as_of(Utc.timestamp_opt(0, 0).unwrap());
        let at_fire = b.lookup_attempt_state("EUR/USD", "o1", None).await.unwrap();
        assert!(
            matches!(at_fire, AttemptState::Pending),
            "fire bar can't fill the resting order, but it's resting → Pending, got {at_fire:?}"
        );

        // As-of bar 1: filled, not yet stopped → open.
        b.set_as_of(Utc.timestamp_opt(3600, 0).unwrap());
        let early = b.lookup_attempt_state("EUR/USD", "o1", None).await.unwrap();
        assert!(
            matches!(early, AttemptState::OpenPosition { .. }),
            "filled on bar 1, not yet stopped → open, got {early:?}"
        );

        // As-of bar 2: SL hit → closed.
        b.set_as_of(Utc.timestamp_opt(7200, 0).unwrap());
        let late = b.lookup_attempt_state("EUR/USD", "o1", None).await.unwrap();
        assert!(
            matches!(late, AttemptState::ClosedLossOrBreakeven { .. }),
            "SL hit by bar 2 → closed, got {late:?}"
        );
    }

    // --- PR 3: list_pending_orders fidelity (shared pending-lifecycle) ---
    //
    // The shared `pending_order_lifecycle` (core) lists broker pending orders to
    // decide what to cancel through a spread hour. Before PR 3 this mock always
    // returned `[]`, so the lifecycle was a no-op offline — replay could never
    // reproduce the live cancel/restore. These pin the fidelity: a still-resting
    // attempt IS reported (so the lifecycle can act on it) and one that filled or
    // was cancelled is NOT (it's no longer resting).

    #[tokio::test]
    async fn list_pending_reports_a_resting_order() {
        // Same geometry as `open_then_closed_...`: at the fire bar the short-stop
        // is placed but not yet filled → a live resting order → must appear in
        // list_pending_orders with its resolved trigger + is_stop.
        let fire_bar = candle(0, 1.1010);
        let fill_bar = candle(3600, 1.1000);
        let b = ReplayBroker::new(vec![fire_bar, fill_bar], 0.0001);
        let shell = Shell::from_candle(&fire_bar.mid());
        b.record_attempt("o1".into(), short_enter_intent(), shell);

        b.set_as_of(Utc.timestamp_opt(0, 0).unwrap());
        let pendings = b.list_pending_orders("").await.unwrap();
        assert_eq!(pendings.len(), 1, "resting order must be reported");
        let o = &pendings[0];
        assert_eq!(o.order_id, "o1");
        assert_eq!(o.instrument, "EUR/USD");
        assert!(o.is_stop, "the intent is a stop entry");
        assert!(
            (o.trigger - 1.1000).abs() < 1e-9,
            "trigger resolves to the absolute 1.1000 stop level, got {}",
            o.trigger,
        );
    }

    #[tokio::test]
    async fn list_pending_drops_filled_and_cancelled_orders() {
        // Once the order fills (as-of the fill bar it's an OpenPosition, not
        // resting) it must NOT appear; and a cancelled order never appears.
        let fire_bar = candle(0, 1.1010);
        let fill_bar = candle(3600, 1.1000); // bid reaches the 1.1000 sell-stop
        let b = ReplayBroker::new(vec![fire_bar, fill_bar], 0.0001);
        let shell = Shell::from_candle(&fire_bar.mid());
        b.record_attempt("o1".into(), short_enter_intent(), shell);

        // As-of the fill bar → filled → not resting → not listed.
        b.set_as_of(Utc.timestamp_opt(3600, 0).unwrap());
        assert!(
            b.list_pending_orders("").await.unwrap().is_empty(),
            "a filled (open) order is no longer resting"
        );

        // Cancel it, rewind to the fire bar → cancelled overrides → not listed.
        b.cancel_order("", "o1").await.unwrap();
        b.set_as_of(Utc.timestamp_opt(0, 0).unwrap());
        assert!(
            b.list_pending_orders("").await.unwrap().is_empty(),
            "a cancelled order is never resting"
        );
    }

    // --- PR 4b-1: position-ledger shadow parity ------------------------------
    //
    // The ledger's `realized_outcome(order_id)` must reproduce the report's
    // current `resolve_fire_any` outcome BIT-FOR-BIT (same fill_at, entry_price,
    // until, SL/TP, and equivalent kind) before 4b-2 switches the report over to
    // it. These build representative fires (fill→TP, fill→SL, never-fill,
    // fill→reversal-close) and assert the two agree — the 4b-1 gate.

    use super::super::replay::{EnterGateOutcome, Fire};
    use super::super::report::resolve_fire_any;
    use trade_control_engine::{FiredIntent, Granularity, TradePlan};

    /// A long stop-entry enter anchored to absolute levels (no signal geometry
    /// needed): buys through the 1.1000 stop, SL 1.0950, TP 1.1100. Built from
    /// YAML like the report tests, so the `Intent` literal stays out of the way.
    fn long_stop_enter() -> Intent {
        serde_yaml::from_str(
            "
            v: 1
            id: t-enter
            trade_id: t
            not_after: \"2026-06-30T00:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: stop, from: close, offset_pips: 0.0, at: 1.1000 }
            stop_loss: { absolute: 1.0950 }
            take_profit: { absolute: 1.1100 }
            risk_pct: 1.0
        ",
        )
        .expect("valid long enter intent")
    }

    /// A minimal H1 plan for EUR_USD at 0.0001 pip — only the fields
    /// `resolve_fire_any` / the ledger read matter (pip_size, tick fallback).
    fn ledger_plan() -> TradePlan {
        TradePlan {
            trade_id: "t".into(),
            instrument: "EUR_USD".into(),
            direction: Direction::Long,
            granularity: Granularity::H1,
            pip_size: 0.0001,
            rules: Vec::new(),
            shadow: false,
            cross_buffer_pct: 0.0,
            retest_atr_step: trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            replay_start: None,
            armed_at: None,
            armed_sentiment: None,
        }
    }

    /// Build an enter `Fire` (as `run_enter` would have placed it) over `forward`,
    /// with the given placed order id. The signal is `None` (a stop enter carries
    /// no latched Pine signal), so the shell is the plain candle shell — exactly
    /// the branch both `resolve_fire_any` and the ledger take.
    fn placed_enter_fire(intent: Intent, forward: Vec<BidAskCandle>, order_id: &str) -> Fire {
        let fire_candle = forward.first().expect("non-empty forward").mid();
        Fire {
            fired: FiredIntent {
                rule_id: "05-enter".into(),
                intent,
                candle: fire_candle,
                signal: None,
            },
            forward,
            gate_outcome: EnterGateOutcome::Placed {
                order_id: Some(order_id.to_string()),
            },
            superseded: false,
            entry_spread_price: None,
        }
    }

    /// Feed a fire's geometry into the broker ledger (via `record_order`) exactly
    /// as the loop will in 4b-2, then compare `realized_outcome` to the report's
    /// `resolve_fire_any` for the same fire. Asserts every load-bearing field.
    fn assert_shadow_parity(fire: &Fire, closes: &[CloseFire], order_id: &str) {
        let plan = ledger_plan();
        let expected = resolve_fire_any(&plan, fire, closes).expect("report resolves this enter");

        let broker = ReplayBroker::new(fire.forward.clone(), plan.pip_size);
        let shell = Shell::from_candle(&fire.fired.candle);
        broker.record_order(
            order_id.into(),
            fire.fired.intent.clone(),
            shell,
            fire.forward.clone(),
            fire.entry_spread_price,
            closes.to_vec(),
        );
        let got = broker
            .realized_outcome(order_id)
            .expect("ledger realizes this order");

        assert_eq!(got.kind, expected.kind, "kind must match the report");
        assert_eq!(got.direction, expected.direction, "direction");
        assert_eq!(got.fill_at, expected.fill_at, "fill_at");
        assert_eq!(got.until, expected.until, "until (box right edge)");
        assert!(
            (got.entry_price - expected.entry_price).abs() < 1e-12,
            "entry_price {} vs {}",
            got.entry_price,
            expected.entry_price
        );
        assert!(
            (got.stop_loss - expected.stop_loss).abs() < 1e-12,
            "stop_loss {} vs {}",
            got.stop_loss,
            expected.stop_loss
        );
        assert!(
            (got.take_profit - expected.take_profit).abs() < 1e-12,
            "take_profit {} vs {}",
            got.take_profit,
            expected.take_profit
        );
    }

    #[test]
    fn shadow_parity_fill_then_take_profit() {
        // Fire below the 1.1000 stop, rise through it (fill on the ask), then on
        // to the 1.1100 TP (exit on the bid).
        let forward = vec![
            candle(0, 1.0980),
            candle(3600, 1.1010), // ask reaches the 1.1000 buy-stop → fill
            candle(7200, 1.1110), // bid reaches the 1.1100 TP → exit
        ];
        let fire = placed_enter_fire(long_stop_enter(), forward, "o-tp");
        assert_shadow_parity(&fire, &[], "o-tp");
    }

    #[test]
    fn shadow_parity_fill_then_stop_loss() {
        // Fill on bar 1, then drop through the 1.0950 SL on bar 2.
        let forward = vec![
            candle(0, 1.0980),
            candle(3600, 1.1010), // fill
            candle(7200, 1.0949), // bid through the 1.0950 SL → stopped
        ];
        let fire = placed_enter_fire(long_stop_enter(), forward, "o-sl");
        assert_shadow_parity(&fire, &[], "o-sl");
    }

    #[test]
    fn shadow_parity_never_filled() {
        // Price never reaches the 1.1000 stop — a resting order that never fills.
        let forward = vec![
            candle(0, 1.0980),
            candle(3600, 1.0985),
            candle(7200, 1.0990),
        ];
        let fire = placed_enter_fire(long_stop_enter(), forward, "o-nf");
        assert_shadow_parity(&fire, &[], "o-nf");
    }

    #[test]
    fn shadow_parity_fill_then_reversal_close() {
        // Fill on bar 1, then a reversal-close fires on bar 2 (before any SL/TP),
        // flattening the position at the close bar's price. Both sides must apply
        // the reversal-close post-pass identically.
        let forward = vec![
            candle(0, 1.0980),
            candle(3600, 1.1010),  // fill
            candle(7200, 1.1020),  // still open (no SL/TP touch)
            candle(10800, 1.1030), // still open
        ];
        let fire = placed_enter_fire(long_stop_enter(), forward, "o-rev");
        // A reversal-close at bar 2 (3600 after fill), price 1.1015.
        let closes = vec![CloseFire {
            at: Utc.timestamp_opt(7200, 0).unwrap(),
            price: 1.1015,
        }];
        assert_shadow_parity(&fire, &closes, "o-rev");
    }

    #[test]
    fn shadow_parity_still_open_at_window_end() {
        // Fill on bar 1, never exits within the window → Open, box runs to the
        // last forward bar. Exercises the `until = window_end` branch.
        let forward = vec![
            candle(0, 1.0980),
            candle(3600, 1.1010), // fill
            candle(7200, 1.1020),
        ];
        let fire = placed_enter_fire(long_stop_enter(), forward, "o-open");
        assert_shadow_parity(&fire, &[], "o-open");
    }

    #[tokio::test]
    async fn realized_outcome_is_none_for_a_cancelled_order() {
        // A cancelled ledger order has no realized outcome — the lifecycle-cancel
        // case later stages drive into a "no fill". (The forward path would
        // otherwise fill-and-TP, so this proves the cancel overrides the physics.)
        let forward = vec![
            candle(0, 1.0980),
            candle(3600, 1.1010),
            candle(7200, 1.1110),
        ];
        let fire = placed_enter_fire(long_stop_enter(), forward.clone(), "o-cancel");
        let broker = ReplayBroker::new(forward.clone(), 0.0001);
        let shell = Shell::from_candle(&fire.fired.candle);
        broker.record_order(
            "o-cancel".into(),
            fire.fired.intent.clone(),
            shell,
            forward,
            None,
            Vec::new(),
        );
        // Sanity: it realizes to a taken outcome before the cancel.
        assert!(broker.realized_outcome("o-cancel").unwrap().kind.is_taken());
        // After cancel: no realized outcome.
        broker.cancel_order("", "o-cancel").await.unwrap();
        assert!(
            broker.realized_outcome("o-cancel").is_none(),
            "a cancelled order has no realized outcome"
        );
    }
}
