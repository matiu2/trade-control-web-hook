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
use trade_control_core::incoming::Verified;
use trade_control_core::intent::{Direction, Intent, Resolved, ResolvedEntry, RiskBudget, Shell};
use trade_control_core::spread_blackout::{elevated_threshold_pips, is_spread_hour};
use trade_control_engine::{BidAskCandle as EngineCandle, SimOutcome, simulate_fill_resolved_zoom};

use super::report::{CloseFire, FillKind};

/// One placed attempt the gate may later ask about, with the geometry needed to
/// re-simulate it. `order_id` is what [`Broker::place_entry`] handed back (the
/// retry gate keys on it); `shell` + `intent` resolve the entry/SL/TP.
#[derive(Clone)]
struct PlacedAttempt {
    order_id: String,
    intent: Intent,
    shell: Shell,
    /// The CONCRETE levels the broker placed this order at — captured verbatim
    /// from the `EntryRequest` `run_enter` handed to `place_entry`. Because
    /// `run_enter` applies the SL-vs-spread floor to `resolved.stop_loss`
    /// *before* building the request, these are the FINAL floored levels the
    /// real broker rests on. Storing them here (instead of re-deriving the floor
    /// off a trailing spread every time the order is queried) is what makes the
    /// sim broker "orders are state": every later question (`resolve`,
    /// `realized_outcome`) tests price against THESE, so the retry-gate state and
    /// the P&L ledger can't disagree (replay↔live divergence #4). `None` only for
    /// an attempt recorded outside the `place_entry` path (the direct-record unit
    /// tests + a legacy re-drive), which fall back to resolving from the intent.
    placed: Option<PlacedLevels>,
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
    // Consumed by the report (4b-2): the loop attaches geometry via `record_order`
    // and the report reads `realized_outcome` instead of re-simulating the fill.
    ledger: Option<LedgerGeometry>,
}

/// The CONCRETE order levels the broker placed an attempt at — the floored stop,
/// the take-profit, and the resolved entry — captured verbatim from the
/// [`EntryRequest`] `run_enter` handed to [`Broker::place_entry`]. These are the
/// single source of truth for every later fill/exit question: the sim walks price
/// against THESE, never re-deriving the SL-vs-spread floor. (`run_enter` already
/// floored `stop_loss` before building the request, so `stop_loss` here is the
/// final placed level.)
#[derive(Clone)]
struct PlacedLevels {
    entry: ResolvedEntry,
    stop_loss: f64,
    take_profit: f64,
}

/// The per-order geometry the position ledger advances a realized outcome
/// against — the forward bid/ask path from the fire bar (the fill sim input and
/// the `until` window-end anchor). The SL/TP/entry LEVELS no longer live here;
/// they're the order's stored [`PlacedLevels`], so the ledger walks the same
/// placed stop the retry-gate `resolve` does (no trailing-spread re-derivation —
/// replay↔live divergence #4). The reversal-close fires are **not** stored here —
/// they're a plan-wide fire-set passed to [`ReplayBroker::realized_outcome`].
#[derive(Clone)]
struct LedgerGeometry {
    /// Bid/ask candles at/after the fire bar (ascending) — the fill sim input
    /// and the `until` window-end anchor (`forward.last()`).
    forward: Vec<EngineCandle>,
}

/// A reversal-close that flattened an open ledger position before its SL/TP —
/// the [`apply_reversal_close`] verdict, carrying the fill it applies to and the
/// close bar/price. Mirrors `report.rs`'s private `ReplayOutcome::ClosedOnReversal`.
struct ReversalClose {
    fill_at: DateTime<Utc>,
    entry_price: f64,
    exit_at: DateTime<Utc>,
    exit_price: f64,
}

/// Exact equality of two resolved entries — same variant, same price. Used to
/// match a lifecycle re-drive `EntryRequest` back to the cancelled attempt it
/// restores; both sides resolve from the SAME intent+shell, so the f64s are
/// identical (no tolerance). A cross-variant pair (stop vs limit) never matches.
fn entries_match(a: &ResolvedEntry, b: &ResolvedEntry) -> bool {
    match (a, b) {
        (ResolvedEntry::Stop { trigger_price: x }, ResolvedEntry::Stop { trigger_price: y })
        | (ResolvedEntry::Limit { trigger_price: x }, ResolvedEntry::Limit { trigger_price: y })
        | (
            ResolvedEntry::Market { reference_price: x },
            ResolvedEntry::Market { reference_price: y },
        ) => x == y,
        _ => false,
    }
}

/// Whether a `06-close-on-reversal` fire flattens this outcome's open position
/// before its own SL/TP — a **verbatim** lift of `report.rs::apply_reversal_close`
/// so the ledger's reversal handling matches the report bit-for-bit. A close bar
/// after the fill (and strictly before any SL/TP exit) closes the position; the
/// earliest such close wins. `None` ⇒ no reversal applies (untaken outcomes, or a
/// close that lands outside the open window).
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
        SimOutcome::NeverFilled | SimOutcome::Declined { .. } | SimOutcome::Unresolved(_) => {
            return None;
        }
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
/// broker-owned equivalent of the report's `FireResult`. Carries the same
/// load-bearing fields `resolve_fire_any` produces, which the report reads
/// (4b-2) instead of re-simulating: direction, the fill bar + price, the box's
/// right edge, the (floored) SL/TP, and the taken/closed kind.
///
/// A cancelled order has no realized outcome — `realized_outcome` returns `None`
/// for it, which is the whole point of the ledger (a spread-hour cancel later
/// flows into a "no fill" here).
#[derive(Debug, Clone, PartialEq)]
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
    /// The price the position actually exited at — the SL price for a
    /// `StoppedOut` (or the break-even price when SL→BE moved it to entry), the
    /// TP price for a `TookProfit`, or the reversal-close bar price for a
    /// `ClosedOnReversal`. `None` for a still-`Open` position (no exit yet) or a
    /// not-taken kind (`NeverFilled` / `Declined` / `SpreadBlackout`). The report
    /// scores R off THIS (`realized_r(entry, stop_loss, exit_price)`) so the
    /// journal's Net R comes from the broker ledger, not a re-simulation.
    pub exit_price: Option<f64>,
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

/// A pre-fetched **finer-granularity** bid/ask series the fill sim zooms into
/// when a coarse exit bar straddles both SL and TP (PR-2 sub-bar zoom). The
/// replay driver pulls this once (e.g. M1 under an H1 plan) over the same span
/// and hands it in; the broker filters it to the ambiguous bar's window through
/// its [`trade_control_engine::SubBars`] impl. Empty ⇒ no zoom (every fill/exit
/// question degrades to the pessimistic-stop assumption, unchanged from PR-1).
struct FinerSeries {
    /// Ascending finer bid/ask candles spanning the same window as `candles`.
    candles: Vec<EngineCandle>,
}

impl trade_control_engine::SubBars for FinerSeries {
    fn sub_bars(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<EngineCandle> {
        self.candles
            .iter()
            .filter(|c| c.time >= start && c.time < end)
            .cloned()
            .collect()
    }
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
    /// The pre-fetched finer series for sub-bar zoom (PR-2), or [`NoZoom`] when
    /// the driver didn't supply one. Every fill/exit path passes this to
    /// `simulate_fill_resolved_zoom`, so an ambiguous SL/TP bar is disambiguated
    /// by finer candles when available and pessimistic-stopped otherwise.
    finer: Option<FinerSeries>,
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
            finer: None,
        }
    }

    /// Attach a pre-fetched finer-granularity bid/ask series for the sub-bar zoom
    /// (PR-2). The driver pulls this once over the same span as the coarse window
    /// and calls this before the replay loop; from then on an exit bar that
    /// straddles both SL and TP is disambiguated by the finer candles instead of
    /// pessimistically assuming the stop. Empty / not called ⇒ pessimistic stop,
    /// exactly as PR-1.
    pub fn with_sub_bars(mut self, finer: Vec<EngineCandle>) -> Self {
        self.finer = Some(FinerSeries { candles: finer });
        self
    }

    /// The [`SubBars`](trade_control_engine::SubBars) provider the sim consults on
    /// an ambiguous bar: the attached finer series, or [`NoZoom`] when none was
    /// supplied. Borrowing the field as a trait object keeps every fill/exit call
    /// site uniform (`simulate_fill_resolved_zoom(.., self.zoom())`).
    fn zoom(&self) -> &dyn trade_control_engine::SubBars {
        match &self.finer {
            Some(f) => f,
            None => &trade_control_engine::NoZoom,
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
    /// the same id when it `record_placement`s). `placed` are the concrete
    /// levels `place_entry` captured from the `EntryRequest` — the floored stop
    /// the broker rests on (`None` only on the direct-record test path, which
    /// falls back to resolving from the intent).
    fn record_attempt(
        &self,
        order_id: String,
        intent: Intent,
        shell: Shell,
        placed: Option<PlacedLevels>,
    ) {
        self.placed.borrow_mut().push(PlacedAttempt {
            order_id,
            intent,
            shell,
            placed,
            cancelled: false,
            ledger: None,
        });
    }

    /// Attach the forward-path geometry to a placed order so a later
    /// [`ReplayBroker::realized_outcome`] can advance it. `forward` is the fire
    /// bar onward — the fill-sim input. The SL/TP levels come from the order's
    /// stored [`PlacedLevels`], not from here (no trailing spread). The
    /// reversal-close fires are a plan-wide set passed to `realized_outcome`.
    ///
    /// If an attempt with this `order_id` already exists (the usual path: the
    /// dispatch's `place_entry` recorded it first, WITH its placed levels), its
    /// ledger is **upgraded** in place — so there's exactly one attempt per order
    /// and its placed levels are preserved. Otherwise a fresh attempt is pushed
    /// with `placed: None` (the direct-record unit-test path, which resolves the
    /// bracket from the intent).
    pub fn record_order(
        &self,
        order_id: String,
        intent: Intent,
        shell: Shell,
        forward: Vec<EngineCandle>,
    ) {
        let geometry = LedgerGeometry { forward };
        let mut placed = self.placed.borrow_mut();
        match placed.iter_mut().find(|a| a.order_id == order_id) {
            Some(existing) => existing.ledger = Some(geometry),
            None => placed.push(PlacedAttempt {
                order_id,
                intent,
                shell,
                placed: None,
                cancelled: false,
                ledger: Some(geometry),
            }),
        }
    }

    /// The armed [`Verified`] (intent + firing shell) the broker holds for a
    /// placed order — the offline seam the shared `pending_order_lifecycle` needs
    /// to cancel/re-drive a resting order WITHOUT an HMAC-signed body (PR 4b-3).
    /// The fake broker already recorded the intent+shell at placement, so a
    /// replay-side `VerifiedSource` reads this instead of `parse_and_verify`.
    ///
    /// The intent's `pip_size` is guaranteed present — the lifecycle's cancel side
    /// (`try_cancel_one`) refuses to cancel an order whose intent has no usable
    /// pip (it needs it to key the record's OFF-side pips math). The plan's baked
    /// `pip_size` is stamped on when the intent didn't carry its own, mirroring
    /// how `dispatch_config` / `run_enter` fall back to the plan pip in replay.
    /// `None` only for an **unknown** order id — a cancelled order still exposes
    /// its armed Verified, because the lifecycle's restore side re-drives it
    /// *after* the cancel (the cancel flag gates the fill outcome, not the payload
    /// seam).
    pub fn armed_verified(&self, order_id: &str) -> Option<Verified> {
        let placed = self.placed.borrow();
        let attempt = placed.iter().find(|a| a.order_id == order_id)?;
        let mut intent = attempt.intent.clone();
        if !intent.pip_size.is_some_and(|p| p > 0.0 && p.is_finite()) {
            intent.pip_size = Some(self.pip_size);
        }
        Some(Verified {
            shell: attempt.shell.clone(),
            intent,
        })
    }

    /// Re-activate the resting order a spread-hour cancel took down, matched by an
    /// incoming re-drive [`EntryRequest`] (PR 4b-3 restore). The lifecycle
    /// re-drives a cancelled order through `run_enter` → `place_entry`; that
    /// request carries the bracket resolved from the SAME recovered intent+shell
    /// the broker armed originally, so an exact match on
    /// `(instrument, direction, entry, stop_loss, take_profit)` against a
    /// `cancelled` attempt identifies it unambiguously (identical inputs → identical
    /// f64s — no tolerance needed). On a match: flip `cancelled` back to false and
    /// return its existing `order_id`; the resting order is restored and the ledger
    /// resolves it normally against its forward path (fills on the next clean bar,
    /// the spike bar still skipped by `find_fill`). `None` when nothing matches.
    fn reactivate_matching_cancelled(&self, req: &EntryRequest<'_>) -> Option<String> {
        let mut placed = self.placed.borrow_mut();
        let matched = placed.iter_mut().find(|a| {
            if !a.cancelled {
                return false;
            }
            if a.intent.instrument != req.instrument {
                return false;
            }
            // Resolve the attempt's bracket the same way the report/ledger do; a
            // resolution failure can't match a resolved request.
            let tick = a.intent.tick_size.unwrap_or(self.pip_size);
            let Ok(resolved) = Resolved::from_intent(&a.intent, &a.shell, self.pip_size, tick)
            else {
                return false;
            };
            // Match on the STABLE identity of the resting order: instrument +
            // direction + entry trigger. The entry trigger is anchored to the
            // signal (e.g. `signal_low`) and is byte-identical between the original
            // placement and the restore. SL/TP are deliberately NOT compared: the
            // restore re-drives `run_enter`, which re-applies the spread-SL floor at
            // the *restore* bar, so the re-floored SL legitimately differs from the
            // original placement's floored (or the stored intent's signed) SL. There
            // is exactly one resting order per cancelled attempt, so entry-trigger
            // identity is unambiguous without the SL/TP tie-break.
            resolved.direction == req.direction && entries_match(&resolved.entry, &req.entry)
        })?;
        matched.cancelled = false;
        // The restore re-drove `run_enter`, which re-applied the SL-spread floor at
        // the *restore* bar — so the re-placed order rests on the fresh request's
        // (re-floored) levels. Refresh the stored levels to match, exactly as a
        // real broker holds the re-placed order's SL/TP.
        matched.placed = Some(PlacedLevels {
            entry: req.entry.clone(),
            stop_loss: req.stop_loss,
            take_profit: req.take_profit,
        });
        tracing::info!(
            order_id = %matched.order_id,
            instrument = %matched.intent.instrument,
            "ReplayBroker: re-activated a spread-hour-cancelled resting order (lifecycle restore)"
        );
        Some(matched.order_id.clone())
    }

    /// The concrete bracket a placed order rests on — its stored [`PlacedLevels`]
    /// folded onto a resolved intent (the "orders are state" bracket the ledger
    /// and retry-gate both walk). The report reads this so its placed-line /
    /// break-even / System-2-widen DISPLAY lines annotate the SAME floored stop
    /// the broker holds, instead of re-deriving the floor off a trailing spread.
    /// `None` when the order isn't found or its intent can't resolve.
    pub fn placed_bracket(&self, order_id: &str) -> Option<Resolved> {
        let placed = self.placed.borrow();
        let attempt = placed.iter().find(|a| a.order_id == order_id)?;
        // The forward path only matters for the `None`-placed fallback floor;
        // a real placed order has captured levels, so an empty slice is fine.
        self.resolved_for_sim(attempt, &[])
    }

    /// The realized outcome of a ledger-tracked order — the broker-owned
    /// equivalent of `report.rs::resolve_fire_any`'s taken/closed verdict for the
    /// same enter. Advances the order's forward path through the SAME engine
    /// physics the report used, in the SAME per-bar precedence:
    ///
    ///   fill → strategy-side/simulator SL/TP → reversal-close → break-even
    ///
    /// (break-even is folded into `simulate_fill_windowed`, and the SL floor into
    /// `apply_entry_spread_floor` — so this driver just calls them in report order).
    /// `closes` is the plan-wide `06-close-on-reversal` fire-set (collected after
    /// the loop by the report); a reversal in it that lands while the position is
    /// open flattens it before its SL/TP.
    ///
    /// Returns `None` when the order was **cancelled** (no fill, no outcome — the
    /// lifecycle-cancel case later stages exercise), wasn't recorded with ledger
    /// geometry (a plain retry-gate attempt), or its bracket can't resolve
    /// (`Unresolved` — nothing to draw, exactly as the report returned `None`).
    pub fn realized_outcome(
        &self,
        order_id: &str,
        closes: &[CloseFire],
    ) -> Option<RealizedOutcome> {
        let placed = self.placed.borrow();
        let attempt = placed.iter().find(|a| a.order_id == order_id)?;
        // A cancelled order never fills — no realized outcome (the whole point of
        // the ledger: a spread-hour cancel flows into "no fill" here).
        if attempt.cancelled {
            return None;
        }
        let geo = attempt.ledger.as_ref()?;
        self.realize(attempt, geo, closes)
    }

    /// Compute a ledger order's realized outcome by walking its STORED placed
    /// bracket (via [`resolved_for_sim`]) forward with [`simulate_fill_resolved`],
    /// then the reversal-close post-pass — and map to a [`RealizedOutcome`]. The
    /// levels come from the order's [`PlacedLevels`], the SAME ones the retry-gate
    /// `resolve` and the report walk, so all three agree (no trailing-spread
    /// re-derivation — replay↔live divergence #4).
    ///
    /// `None` when the intent can't resolve — the report drew nothing there too.
    fn realize(
        &self,
        attempt: &PlacedAttempt,
        geo: &LedgerGeometry,
        closes: &[CloseFire],
    ) -> Option<RealizedOutcome> {
        let pip_size = self.pip_size;
        let intent = &attempt.intent;
        let shell = &attempt.shell;
        // The placed bracket the position rests on — the stored floored levels,
        // not a fresh floor. `None` ⇒ the intent couldn't resolve (the report's
        // `.ok()?` drew nothing).
        let resolved = self.resolved_for_sim(attempt, &geo.forward)?;
        let direction = resolved.direction;
        let stop_loss = resolved.stop_loss;
        let take_profit = resolved.take_profit;
        let placed_level = resolved.entry.reference_price();
        // The not-taken / open box runs to the last forward bar; a closed trade
        // overrides `until` with its exit bar below.
        let window_end = geo.forward.last().map(|c| c.time).unwrap_or(shell.time);
        let fire_at = shell.time;

        let raw = simulate_fill_resolved_zoom(
            &resolved,
            intent,
            shell,
            pip_size,
            &geo.forward,
            self.zoom(),
        );
        let (fill_at, until, entry_price, exit_price, kind) =
            match apply_reversal_close(&raw, closes) {
                Some(rc) => (
                    rc.fill_at,
                    rc.exit_at,
                    rc.entry_price,
                    Some(rc.exit_price),
                    FillKind::ClosedOnReversal,
                ),
                None => match &raw {
                    SimOutcome::FilledOpen {
                        fill_at,
                        entry_price,
                    } => (*fill_at, window_end, *entry_price, None, FillKind::Open),
                    // `exit_price` carries the ACTUAL exit — the break-even price when
                    // SL→BE moved the stop to entry, else the floored SL — so the
                    // report's R (`realized_r(entry, stop_loss, exit_price)`) matches
                    // the sim without re-deriving break-even off stale geometry.
                    SimOutcome::StoppedOut {
                        fill_at,
                        entry_price,
                        exit_at,
                        exit_price,
                    } => (
                        *fill_at,
                        *exit_at,
                        *entry_price,
                        Some(*exit_price),
                        FillKind::StoppedOut,
                    ),
                    SimOutcome::TookProfit {
                        fill_at,
                        entry_price,
                        exit_at,
                        exit_price,
                    } => (
                        *fill_at,
                        *exit_at,
                        *entry_price,
                        Some(*exit_price),
                        FillKind::TookProfit,
                    ),
                    SimOutcome::NeverFilled => (
                        fire_at,
                        window_end,
                        placed_level,
                        None,
                        FillKind::NeverFilled,
                    ),
                    SimOutcome::Declined { .. } => {
                        (fire_at, window_end, placed_level, None, FillKind::Declined)
                    }
                    // `Unresolved` has nothing to draw — the report returned `None`
                    // from its `Unresolved => return None` arm, so the ledger does too.
                    SimOutcome::Unresolved(_) => return None,
                },
            };
        Some(RealizedOutcome {
            direction,
            fill_at,
            until,
            entry_price,
            stop_loss,
            take_profit,
            exit_price,
            kind,
        })
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

    /// The `Resolved` bracket the sim walks for an attempt — its stored PLACED
    /// levels (the floored stop/TP/entry the broker rests on), NOT a fresh
    /// re-derivation of the SL-vs-spread floor. Resolves the intent+shell first
    /// (for direction / break-even / min_r — the non-level fields), then
    /// overwrites entry/stop_loss/take_profit with the stored [`PlacedLevels`].
    /// This is the "orders are state" core: every fill/exit question walks the
    /// SAME placed levels, so the retry-gate `resolve` and the ledger `realize`
    /// can't disagree (replay↔live divergence #4). `None` when the intent can't
    /// resolve.
    ///
    /// Fallback (`attempt.placed == None`): the direct-record test path and a
    /// legacy re-drive have no captured request, so resolve from the intent and
    /// apply the entry-spread floor exactly as before — behaviour-preserving for
    /// those callers.
    fn resolved_for_sim(
        &self,
        attempt: &PlacedAttempt,
        forward: &[BidAskCandle],
    ) -> Option<Resolved> {
        let tick = attempt.intent.tick_size.unwrap_or(self.pip_size);
        let mut resolved =
            Resolved::from_intent(&attempt.intent, &attempt.shell, self.pip_size, tick).ok()?;
        match &attempt.placed {
            Some(p) => {
                // The broker rests on the captured levels — overwrite the resolved
                // (signed, un-floored) ones. No spread, no floor: the placement
                // already floored the stop.
                resolved.entry = p.entry.clone();
                resolved.stop_loss = p.stop_loss;
                resolved.take_profit = p.take_profit;
            }
            None => {
                // Legacy/test path: no captured request → floor from the intent as
                // the pre-"orders-are-state" code did (fire-bar spread).
                trade_control_engine::apply_entry_spread_floor(
                    &mut resolved,
                    self.pip_size,
                    forward,
                    None,
                );
            }
        }
        Some(resolved)
    }

    /// Resolve a placed attempt's current state from its price path up to
    /// `as_of`. The attempt's own candles are those at/after its shell time
    /// (the bar it fired on) within the bounded window.
    fn resolve(&self, attempt: &PlacedAttempt) -> AttemptState {
        if attempt.cancelled {
            return AttemptState::Cancelled;
        }
        let window = self.window_to_as_of();
        // Forward path = candles from the firing bar onward (the sim walks these
        // to find the fill, then the SL/TP touch — against the STORED levels).
        let forward: Vec<BidAskCandle> = window
            .into_iter()
            .filter(|c| c.time >= attempt.shell.time)
            .collect();
        let Some(resolved) = self.resolved_for_sim(attempt, &forward) else {
            // Unresolvable intent → no order went on; the slot is free.
            return AttemptState::Cancelled;
        };
        match simulate_fill_resolved_zoom(
            &resolved,
            &attempt.intent,
            &attempt.shell,
            self.pip_size,
            &forward,
            self.zoom(),
        ) {
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
            // (`expiry_bars`-driven expiry is folded into the fill window, so an
            // expired order resolves to `NeverFilled`/`Pending` here too — these
            // v2 plans don't set `expiry_bars`, and the gate's cap/window bound
            // the re-entry count regardless.)
            SimOutcome::NeverFilled => AttemptState::Pending,
            // `simulate_fill_resolved` walks an already-resolved bracket, so it
            // never returns `Declined` (the SL-floor reject) or `Unresolved` (the
            // resolve failure) — those are handled by the `resolved_for_sim` guard
            // above. Kept for match exhaustiveness → the slot is free.
            SimOutcome::Declined { .. } | SimOutcome::Unresolved(_) => AttemptState::Cancelled,
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
        max_risk_pct: f64,
        max_open_positions: u32,
        req: &EntryRequest<'_>,
    ) -> Result<String, EntryError> {
        // Enforce the two account caps the real broker enforces AND the replay
        // can faithfully reproduce offline — so a live reject-at-cap is not
        // silently taken as a fill (bug ③). Both mirror the real
        // `broker_oanda::place_entry` decision exactly.
        //
        // 1. Percent risk-cap: a pure comparison, no equity needed — identical
        //    to the pre-equity `RiskBudget::Percent` check the real broker runs.
        //    `Amount` / `Units` need live equity to derive a percent, which the
        //    offline replay doesn't have, so those stay unchecked (conservative:
        //    replay never rejects where it can't know the equity — it can only
        //    ever be rosier-or-equal, never reject a trade live would take).
        if let RiskBudget::Percent(pct) = req.risk
            && pct > max_risk_pct
        {
            return Err(EntryError::RiskCapExceeded {
                requested: pct,
                cap: max_risk_pct,
            });
        }
        // 2. Open-positions cap: count attempts open as-of the fire bar (the same
        //    `resolve`-derived count `list_open_positions` reports) and reject at
        //    the cap, mirroring the real broker's `open_position_count >= cap`.
        //    In a single-plan replay this is that instrument's open count — the
        //    best offline proxy for the account-wide count, and conservative (it
        //    can only reject, never over-fill).
        let open_now = self
            .placed
            .borrow()
            .iter()
            .filter(|a| matches!(self.resolve(a), AttemptState::OpenPosition { .. }))
            .count();
        if open_now as u32 >= max_open_positions {
            return Err(EntryError::OpenPositionsCapExceeded);
        }

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
                // Capture the CONCRETE levels the broker is placing — the floored
                // stop `run_enter` already applied before building this request.
                // Every later fill/exit question walks these, never re-deriving
                // the floor (replay↔live divergence #4).
                let placed = PlacedLevels {
                    entry: req.entry.clone(),
                    stop_loss: req.stop_loss,
                    take_profit: req.take_profit,
                };
                self.record_attempt(a.order_id.clone(), a.intent, a.shell, Some(placed));
                Ok(a.order_id)
            }
            // No armed placement: this is the shared `pending_order_lifecycle`
            // RE-DRIVING a spread-hour-cancelled order (PR 4b-3). The broker
            // already holds that order's `PlacedAttempt` (intent + shell +
            // order_id, `cancelled == true`), so "place it again" means
            // **re-activate** that resting order — flip `cancelled` back to false
            // and hand back its existing id. The order resumes resting and, with
            // the spike bar behind it, fills on the next clean bar (the `find_fill`
            // spread-hour skip still blocks the rubbish-bar fill). This is the
            // broker restoring the resting order the engine told it to re-place —
            // faithful to the cancel→restore→fill sequence the live path runs.
            None => match self.reactivate_matching_cancelled(req) {
                Some(order_id) => Ok(order_id),
                // Neither armed nor a matching cancelled attempt — a genuine
                // wiring fault (an enter dispatched without arming, and not a
                // known re-drive). Fail loudly rather than fabricate an id.
                None => {
                    tracing::error!(
                        "ReplayBroker::place_entry: no armed placement and no matching cancelled \
                         order to re-activate — replay wiring bug"
                    );
                    Err(EntryError::OrderRejected)
                }
            },
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

    async fn get_quote(&self, instrument: &str) -> Result<Quote, LookupError> {
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
        let as_of = *self.as_of.borrow();
        // Inside a baked spread hour, the OVERNIGHT LIQUIDITY TROUGH is wide *by
        // definition* — the whole reason the block exists — even when a particular
        // bar's CLOSE happens to print a narrow spread (the trough is sustained;
        // the close is a noisy sub-sample). The replay has no live tick to know the
        // instantaneous spread, so a real bar's close-spread mid-block is an
        // unreliable recovery signal: it dips narrow on some bars and would make
        // the OFF-side (`pending_lifecycle::off_now`) FALSELY "recover" the trade
        // early, restoring a cancelled resting order that then gets re-cancelled the
        // next in-block bar — a cancel↔restore ping-pong. It also mis-lets an entry
        // fire inside the trough. So in-block we report a spread AT the elevated
        // threshold: the OFF-side stays held until the baked hour ENDS (its stated
        // deterministic off-signal), and the entry gate correctly sees the trough.
        // Out of block, the real close-spread flows through unchanged.
        if is_spread_hour(instrument, as_of)
            && let Some(c) = self.candle_at_as_of()
        {
            let mid = (c.bid_c + c.ask_c) / 2.0;
            let half = elevated_threshold_pips(instrument) * self.pip_size / 2.0;
            return Ok(Quote {
                bid: mid - half,
                ask: mid + half,
            });
        }
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
        b.record_attempt("o1".into(), short_enter_intent(), shell, None);
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
        b.record_attempt("o1".into(), short_enter_intent(), shell, None);

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

    // --- bug ③: place_entry enforces the caps the real broker enforces ---
    //
    // Before this, `place_entry` underscore-ignored `max_risk_pct` /
    // `max_open_positions` and always accepted full size — so replay took an
    // entry the live broker would reject-at-cap. These pin the two caps the
    // replay can faithfully reproduce offline (Percent risk-cap; open-positions
    // count as-of), mirroring `broker_oanda::place_entry`.

    /// An `EntryRequest` for a plain stop entry at the given risk budget.
    fn entry_req(risk: RiskBudget) -> EntryRequest<'static> {
        EntryRequest {
            instrument: "EUR/USD",
            direction: Direction::Short,
            entry: ResolvedEntry::Stop {
                trigger_price: 1.1000,
            },
            stop_loss: 1.1020,
            take_profit: 1.0950,
            risk,
            dry_run: false,
        }
    }

    #[tokio::test]
    async fn place_entry_rejects_a_percent_over_the_risk_cap() {
        let b = ReplayBroker::new(vec![candle(0, 1.1010)], 0.0001);
        b.arm_placement(
            "o1".into(),
            short_enter_intent(),
            Shell::from_candle(&candle(0, 1.1010).mid()),
        );
        // Request 2% against a 1% cap → the same RiskCapExceeded the real broker
        // returns from its pre-equity Percent check.
        let err = b
            .place_entry(1.0, 3, &entry_req(RiskBudget::Percent(2.0)))
            .await
            .unwrap_err();
        assert!(
            matches!(err, EntryError::RiskCapExceeded { .. }),
            "2% over a 1% cap must reject, got {err:?}"
        );
    }

    #[tokio::test]
    async fn place_entry_within_the_risk_cap_is_accepted() {
        let b = ReplayBroker::new(vec![candle(0, 1.1010)], 0.0001);
        b.arm_placement(
            "o1".into(),
            short_enter_intent(),
            Shell::from_candle(&candle(0, 1.1010).mid()),
        );
        let ok = b
            .place_entry(1.0, 3, &entry_req(RiskBudget::Percent(1.0)))
            .await;
        assert_eq!(ok.unwrap(), "o1", "1% at a 1% cap is allowed (not >)");
    }

    #[tokio::test]
    async fn place_entry_rejects_at_the_open_positions_cap() {
        // One position already open as-of the fire bar; cap = 1 → the next
        // place_entry must reject, exactly as the real broker's
        // `open_position_count >= max_open_positions`.
        let fire = candle(0, 1.1010); // above the short-stop trigger, no fill on fire bar
        let fill = candle(3600, 1.1000); // bid reaches the 1.1000 sell-stop → open
        let b = ReplayBroker::new(vec![fire, fill], 0.0001);
        // Attempt #1: recorded + resolves OpenPosition as-of bar 1.
        b.record_attempt(
            "o1".into(),
            short_enter_intent(),
            Shell::from_candle(&fire.mid()),
            None,
        );
        b.set_as_of(Utc.timestamp_opt(3600, 0).unwrap());
        // Sanity: exactly one open now.
        assert_eq!(b.list_open_positions("").await.unwrap().len(), 1);

        // Attempt #2 with cap = 1 → rejected at the cap.
        b.arm_placement(
            "o2".into(),
            short_enter_intent(),
            Shell::from_candle(&fill.mid()),
        );
        let err = b
            .place_entry(1.0, 1, &entry_req(RiskBudget::Percent(1.0)))
            .await
            .unwrap_err();
        assert!(
            matches!(err, EntryError::OpenPositionsCapExceeded),
            "one open + cap 1 must reject the next, got {err:?}"
        );
    }

    #[tokio::test]
    async fn place_entry_under_the_open_positions_cap_is_accepted() {
        // One open, cap = 3 → the next place is allowed.
        let fire = candle(0, 1.1010);
        let fill = candle(3600, 1.1000);
        let b = ReplayBroker::new(vec![fire, fill], 0.0001);
        b.record_attempt(
            "o1".into(),
            short_enter_intent(),
            Shell::from_candle(&fire.mid()),
            None,
        );
        b.set_as_of(Utc.timestamp_opt(3600, 0).unwrap());
        b.arm_placement(
            "o2".into(),
            short_enter_intent(),
            Shell::from_candle(&fill.mid()),
        );
        let ok = b
            .place_entry(1.0, 3, &entry_req(RiskBudget::Percent(1.0)))
            .await;
        assert_eq!(ok.unwrap(), "o2", "one open under a cap of 3 is allowed");
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
        b.record_attempt("o1".into(), short_enter_intent(), shell, None);

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
        b.record_attempt("o1".into(), short_enter_intent(), shell, None);

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
            cross_buffer_atr: 0.0,
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
            // Set by `assert_shadow_parity` after recording (read back from the
            // broker), so the direct-record test path mirrors the driver.
            placed_bracket: None,
            realized: None,
        }
    }

    /// Drive a fire through the broker ledger exactly as the replay loop does
    /// (record its geometry, realize it against the close-fire set, stash the
    /// outcome on the fire), then assert the report's `resolve_fire_any` reads
    /// that outcome back faithfully — every load-bearing field. This is the 4b-2
    /// wiring guarantee: the report is pure formatting of the broker's ledger.
    fn assert_shadow_parity(mut fire: Fire, closes: &[CloseFire], order_id: &str) {
        let plan = ledger_plan();
        let broker = ReplayBroker::new(fire.forward.clone(), plan.pip_size);
        let shell = Shell::from_candle(&fire.fired.candle);
        broker.record_order(
            order_id.into(),
            fire.fired.intent.clone(),
            shell,
            fire.forward.clone(),
        );
        // The loop reads back the placed bracket + stashes the broker's realized
        // outcome on the fire; the report reads both (never re-simulating).
        fire.placed_bracket = broker.placed_bracket(order_id);
        let realized = broker
            .realized_outcome(order_id, closes)
            .expect("ledger realizes this order");
        fire.realized = Some(realized.clone());

        let got = resolve_fire_any(&plan, &fire).expect("report resolves this enter");

        assert_eq!(got.kind, realized.kind, "kind must match the broker");
        assert_eq!(got.direction, realized.direction, "direction");
        assert_eq!(got.fill_at, realized.fill_at, "fill_at");
        assert_eq!(got.until, realized.until, "until (box right edge)");
        assert!(
            (got.entry_price - realized.entry_price).abs() < 1e-12,
            "entry_price {} vs {}",
            got.entry_price,
            realized.entry_price
        );
        assert!(
            (got.stop_loss - realized.stop_loss).abs() < 1e-12,
            "stop_loss {} vs {}",
            got.stop_loss,
            realized.stop_loss
        );
        assert!(
            (got.take_profit - realized.take_profit).abs() < 1e-12,
            "take_profit {} vs {}",
            got.take_profit,
            realized.take_profit
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
        assert_shadow_parity(fire, &[], "o-tp");
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
        assert_shadow_parity(fire, &[], "o-sl");
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
        assert_shadow_parity(fire, &[], "o-nf");
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
        assert_shadow_parity(fire, &closes, "o-rev");
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
        assert_shadow_parity(fire, &[], "o-open");
    }

    /// Divergence #4, dissolved: the retry-gate `resolve` and the ledger both walk
    /// the SAME stored placed stop, so a wick landing BETWEEN the signed SL and the
    /// (wider) placed SL can no longer flip one path's verdict from the other's.
    ///
    /// The short's signed SL is 1.1020; we place it (via the real `place_entry`
    /// path) with a FLOORED stop of 1.1030 in the `EntryRequest` — exactly what
    /// `run_enter` hands the broker after the SL-spread floor widens it. A later
    /// bar's ask wicks to 1.1025 — past the signed 1.1020 but short of the placed
    /// 1.1030. With the placed stop honoured, the position is NOT stopped: both
    /// `resolve` (→ OpenPosition) and `realized_outcome` (→ Open, no exit) agree.
    /// (Pre-"orders-are-state", `resolve` floored off the fire-bar spread and the
    /// ledger off a trailing mean, so a 1.1025 wick could stop one but not the
    /// other — the #4 corner.)
    #[tokio::test]
    async fn resolve_and_realize_agree_on_the_stored_placed_stop() {
        // Explicit books: a short fills when the BID falls to 1.1000, exits (SL)
        // when the ASK rises to the stop. Bar 2's ask peaks at 1.1025.
        let ba = |epoch: i64, bid: f64, ask: f64, bid_h: f64, ask_h: f64| BidAskCandle {
            time: Utc.timestamp_opt(epoch, 0).unwrap(),
            o: (bid + ask) / 2.0,
            h: (bid_h + ask_h) / 2.0,
            l: (bid + ask) / 2.0 - 0.001,
            c: (bid + ask) / 2.0,
            bid_o: bid,
            bid_h,
            bid_l: bid - 0.001,
            bid_c: bid,
            ask_o: ask,
            ask_h,
            ask_l: ask - 0.001,
            ask_c: ask,
        };
        let forward = vec![
            ba(0, 1.1010, 1.1012, 1.1012, 1.1014), // fire bar (short enter fires)
            ba(3600, 1.0998, 1.1000, 1.1002, 1.1004), // bid 1.0998 ≤ 1.1000 → fill
            ba(7200, 1.1021, 1.1023, 1.1023, 1.1025), // ask peaks 1.1025 (past signed 1.1020)
            ba(10800, 1.1000, 1.1002, 1.1004, 1.1006), // still open, no exit
        ];
        let b = ReplayBroker::new(forward.clone(), 0.0001);
        let shell = Shell::from_candle(&forward[0].mid());
        b.arm_placement("o4".into(), short_enter_intent(), shell);
        // Place with the FLOORED stop (1.1030), as `run_enter` would after the
        // SL-spread floor — wider than the signed 1.1020.
        let req = EntryRequest {
            instrument: "EUR/USD",
            direction: Direction::Short,
            entry: ResolvedEntry::Stop {
                trigger_price: 1.1000,
            },
            stop_loss: 1.1030,
            take_profit: 1.0950,
            risk: RiskBudget::Percent(1.0),
            dry_run: false,
        };
        let order_id = b.place_entry(1.0, 3, &req).await.expect("placed");
        // The driver attaches the forward path after placement (the ledger input);
        // this upgrades the existing attempt in place, preserving its placed levels.
        b.record_order(
            order_id.clone(),
            short_enter_intent(),
            Shell::from_candle(&forward[0].mid()),
            forward.clone(),
        );

        // Retry-gate state: as of the wick bar, the position is OPEN (the 1.1025
        // ask never reached the placed 1.1030 stop) — NOT closed-at-loss.
        b.set_as_of(Utc.timestamp_opt(7200, 0).unwrap());
        let states = b.placed.borrow();
        let attempt = states.iter().find(|a| a.order_id == order_id).unwrap();
        assert!(
            matches!(b.resolve(attempt), AttemptState::OpenPosition { .. }),
            "resolve must see the position OPEN against the placed 1.1030 stop, \
             not stopped by the 1.1025 wick past the signed 1.1020"
        );
        drop(states);

        // Ledger: the realized outcome is Open (no exit) — the SAME verdict, off
        // the SAME placed stop. (No reversal-close fires.)
        let realized = b
            .realized_outcome(&order_id, &[])
            .expect("ledger realizes the placed order");
        assert_eq!(
            realized.kind,
            FillKind::Open,
            "ledger must agree: open, not stopped — got {:?}",
            realized.kind
        );
        assert!(
            (realized.stop_loss - 1.1030).abs() < 1e-12,
            "the scored stop is the placed 1.1030, not the signed 1.1020 — got {}",
            realized.stop_loss
        );
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
        broker.record_order("o-cancel".into(), fire.fired.intent.clone(), shell, forward);
        // Sanity: it realizes to a taken outcome before the cancel.
        assert!(
            broker
                .realized_outcome("o-cancel", &[])
                .unwrap()
                .kind
                .is_taken()
        );
        // After cancel: no realized outcome.
        broker.cancel_order("", "o-cancel").await.unwrap();
        assert!(
            broker.realized_outcome("o-cancel", &[]).is_none(),
            "a cancelled order has no realized outcome"
        );
    }
}
