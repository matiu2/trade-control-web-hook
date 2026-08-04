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

use super::fill_sim::{SimOutcome, simulate_fill_resolved_zoom};
use super::report::FillKind;
use chrono::{DateTime, Utc};
use trade_control_core::broker::{
    AmendError, AttemptState, BidAskCandle, Broker, CancelError, Candle, CandleError, EntryError,
    EntryRequest, Granularity, LookupError, OpenPosition, PendingOrder, Quote,
};
use trade_control_core::incoming::Verified;
use trade_control_core::intent::{Direction, Intent, Resolved, ResolvedEntry, RiskBudget, Shell};
use trade_control_core::spread_blackout::{elevated_threshold_pips, is_spread_hour};

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
}

/// The CONCRETE order levels the broker placed an attempt at — the floored stop,
/// the take-profit, and the resolved entry — captured verbatim from the
/// [`EntryRequest`] `run_enter` handed to [`Broker::place_entry`]. These are the
/// single source of truth for every later fill/exit question: the sim walks price
/// against THESE, never re-deriving the SL-vs-spread floor. (`run_enter` already
/// floored `stop_loss` before building the request, so `stop_loss` here is the
/// final placed level.)
#[derive(Clone)]
pub(crate) struct PlacedLevels {
    entry: ResolvedEntry,
    stop_loss: f64,
    take_profit: f64,
}

// ---------------------------------------------------------------------------
// Stateful held model (S1) — the broker HOLDS position state and mutates it as
// bars advance, instead of re-simulating each placed order's price path on every
// query. This is the single source of truth the engine queries exactly like the
// live worker queries the real broker: `close_positions` actually removes a held
// position (no longer a no-op stub), so reversal- and expiry-closes flatten a
// position at the bar the engine dispatches them — killing the two-brain bug
// class (a same-bar fill+reversal, a reversal-closed slot wrongly reported open).
//
// Each held record stores the CONCRETE placed levels (the floored stop verbatim)
// so every fill/exit test walks the same bracket the retry-gate saw — preserving
// "orders are state" / replay↔live divergence #4 in the held model.
// ---------------------------------------------------------------------------

/// A resting order the broker holds: placed by `place_entry`, not yet triggered.
/// Carries everything a later bar needs to test the trigger touch and, on fill,
/// promote it to a [`HeldPosition`] against the same stored levels.
#[derive(Clone)]
struct HeldOrder {
    order_id: String,
    intent: Intent,
    shell: Shell,
    /// The floored levels captured from the `EntryRequest` (as [`PlacedLevels`]),
    /// or `None` for the direct-record/legacy path (floored from the intent).
    placed: Option<PlacedLevels>,
    /// Set when the spread-hour lifecycle cancels this resting order (supersede /
    /// cancel-and-replace). A cancelled resting order fills nothing and appears in
    /// neither the open nor the pending list; a restore re-activates it.
    cancelled: bool,
}

/// A filled position the broker holds: promoted from a [`HeldOrder`] when a bar
/// triggered its entry, not yet closed. Removed (→ [`ClosedTrade`]) on an SL/TP
/// touch, a reversal-close, or an expiry flatten.
#[derive(Clone)]
struct HeldPosition {
    order_id: String,
    intent: Intent,
    shell: Shell,
    /// The bracket the position rests on — the floored stop, take-profit, and the
    /// entry the fill landed at. Never re-derived (divergence #4).
    placed: Option<PlacedLevels>,
    direction: Direction,
    entry_price: f64,
    fill_at: DateTime<Utc>,
}

/// Why a held position left the book — drives the report's exit label and R sign.
#[derive(Clone, Copy, PartialEq)]
pub enum ExitReason {
    /// Stop-loss touched (or SL→break-even scratch when `exit_price ≈ entry`).
    StoppedOut,
    /// Take-profit touched.
    TookProfit,
    /// A gate-passing reversal-close (`06-/07-close-on-…`) flattened it.
    Reversal,
    /// The trade-expiry `close-positions` veto flattened it at wall-clock expiry.
    Expiry,
    /// The structure-invalidation veto (`too-low` for a long / `too-high` for a
    /// short) flattened it at `ClosePositions` level — price ran back past the
    /// shoulder, so the thesis is dead. Distinct from [`Self::Expiry`]: both are
    /// `ClosePositions` vetos, but only one of them is the clock running out.
    /// Conflating them printed "CLOSED AT EXPIRY" for an invalidation close with
    /// the trade-expiry still days away (GBP/NZD iH&S 2026-07-22).
    Invalidation,
}

/// A closed position in the broker's P&L ledger — the terminal record the report
/// reads instead of a post-loop re-simulation pass. Entry/exit/reason are enough
/// to reconstruct R against the stored floored stop.
#[derive(Clone)]
struct ClosedTrade {
    order_id: String,
    direction: Direction,
    entry_price: f64,
    /// The floored stop the position rested on — R is `realized_r(entry, stop, exit)`.
    stop_loss: f64,
    take_profit: f64,
    fill_at: DateTime<Utc>,
    exit_at: DateTime<Utc>,
    exit_price: f64,
    reason: ExitReason,
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
    /// The sub-bar zoom provider (PR-2), or `None` ⇒ [`NoZoom`]. Every fill/exit
    /// path passes this to `simulate_fill_resolved_zoom`, so an ambiguous SL/TP
    /// bar is disambiguated by finer candles when available and
    /// pessimistic-stopped otherwise.
    ///
    /// A trait object rather than a concrete series so the driver can inject
    /// either half of the LAZY two-pass zoom (`super::lazy_zoom`): a
    /// `RecordingSubBars` on pass 1 (serves nothing, records the windows the sim
    /// asks for) and a `WindowSubBars` on pass 2 (serves just those windows).
    /// The broker doesn't care which — it only forwards to the sim.
    finer: Option<Box<dyn super::fill_sim::SubBars>>,

    // --- Stateful held model (S1). Mutated by `advance()` per bar and by
    // `place_entry`/`close_positions`; read by `list_open_positions` /
    // `lookup_attempt_state` / `list_pending_orders` and the P&L readout. During
    // the migration these coexist with the `placed` re-sim path (S3 asserts they
    // agree); the re-sim path is deleted at S8.
    /// Resting orders placed but not yet triggered.
    resting: RefCell<Vec<HeldOrder>>,
    /// Filled positions not yet closed.
    open: RefCell<Vec<HeldPosition>>,
    /// The P&L ledger — closed positions in exit order.
    closed: RefCell<Vec<ClosedTrade>>,
    /// The reason the NEXT `close_positions` call records (Reversal by default;
    /// the loop sets Expiry / Invalidation before dispatching the corresponding
    /// `ClosePositions` veto). Set via `set_close_reason` right before the engine
    /// dispatches a close.
    close_reason: RefCell<ExitReason>,
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
            resting: RefCell::new(Vec::new()),
            open: RefCell::new(Vec::new()),
            closed: RefCell::new(Vec::new()),
            close_reason: RefCell::new(ExitReason::Reversal),
        }
    }

    /// Set the reason the next `close_positions` records. The loop calls this
    /// right before the engine dispatches a close: `Reversal` for a
    /// reversal-close fire, `Expiry` for the trade-expiry ClosePositions veto,
    /// `Invalidation` for the structure-invalidation ClosePositions veto.
    pub fn set_close_reason(&self, reason: ExitReason) {
        *self.close_reason.borrow_mut() = reason;
    }

    /// Attach the sub-bar zoom provider (PR-2) — the seam the LAZY two-pass zoom
    /// uses (`super::lazy_zoom`). Pass 1 injects a `RecordingSubBars` (serves
    /// nothing, records which windows the sim asked for); pass 2 injects a
    /// `WindowSubBars` built from the narrow fetch of exactly those windows.
    /// Not called ⇒ pessimistic stop on an ambiguous bar, exactly as PR-1.
    ///
    /// Deliberately takes a provider, not a candle series: the eager
    /// `with_sub_bars(Vec<EngineCandle>)` it replaced is what made the driver
    /// pull a finer series across the WHOLE coarse window to disambiguate at most
    /// one bar per entry. Keeping it would leave a second, wasteful way to do the
    /// same thing.
    pub fn with_sub_bars_provider(mut self, finer: Box<dyn super::fill_sim::SubBars>) -> Self {
        self.finer = Some(finer);
        self
    }

    /// The [`SubBars`](super::fill_sim::SubBars) provider the sim consults on
    /// an ambiguous bar: the attached finer series, or [`NoZoom`] when none was
    /// supplied. Borrowing the field as a trait object keeps every fill/exit call
    /// site uniform (`simulate_fill_resolved_zoom(.., self.zoom())`).
    fn zoom(&self) -> &dyn super::fill_sim::SubBars {
        match &self.finer {
            Some(f) => f.as_ref(),
            None => &super::fill_sim::NoZoom,
        }
    }

    /// Point all subsequent prior-attempt lookups at `as_of` (the open time of
    /// the bar the gate is evaluating). Call before each `retry_gate::evaluate`.
    ///
    /// **This MUST be a bar-OPEN time, never a bar CLOSE.** Candle timestamps are
    /// bar-open times, so a bar's close equals the NEXT bar's open and the two are
    /// indistinguishable by value — nothing here can detect the mistake. Every
    /// held read (`list_pending_orders` / `list_open_positions` /
    /// `held_attempt_state`) calls `advance(as_of)`, whose `prefix_from_fire`
    /// bound is inclusive, so a close-bounded `as_of` admits the next bar into
    /// the fill window and lets an order placed on bar N fill AND stop against
    /// bar N+1 a whole bar early. That manufactured −1R losses whose presence
    /// depended on the replay's `--start` cursor (BUG-same-bar-fill-and-stop;
    /// Coffee M15 2026-07-21, −0.40R vs −3.00R on the same plan and candles,
    /// from the lifecycle step passing the loop's `now`).
    ///
    /// The replay loop has both values in hand — pass `bar_open`, not `now`.
    /// Rules that legitimately key on the bar close (the lifecycle's spread-hour
    /// gate) take `now` as their own argument and are unaffected by this clock.
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
    pub(crate) fn record_attempt(
        &self,
        order_id: String,
        intent: Intent,
        shell: Shell,
        placed: Option<PlacedLevels>,
    ) {
        // Register a placement: a held resting order the per-bar `advance()` steps
        // to open/closed, plus the retry-gate `PlacedAttempt` record. `placed` are
        // the concrete floored levels captured from the `EntryRequest` (`None` on
        // the direct-record test path → resolved from the intent).
        self.resting.borrow_mut().push(HeldOrder {
            order_id: order_id.clone(),
            intent: intent.clone(),
            shell: shell.clone(),
            placed: placed.clone(),
            cancelled: false,
        });
        self.placed.borrow_mut().push(PlacedAttempt {
            order_id,
            intent,
            shell,
            placed,
            cancelled: false,
        });
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
        let restored_id = matched.order_id.clone();
        let restored_levels = PlacedLevels {
            entry: req.entry.clone(),
            stop_loss: req.stop_loss,
            take_profit: req.take_profit,
        };
        drop(placed);
        // Mirror the restore onto the held resting order: un-cancel it and refresh
        // its levels to the re-floored request, so `advance()` resumes stepping it
        // (the spread-hour fill skip still blocks a rubbish-bar fill).
        if let Some(o) = self
            .resting
            .borrow_mut()
            .iter_mut()
            .find(|o| o.order_id == restored_id)
        {
            o.cancelled = false;
            o.placed = Some(restored_levels);
        }
        Some(restored_id)
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

    /// S5b: the realized outcome READ FROM THE HELD LEDGER — the single source of
    /// truth. Replaces the re-sim `realized_outcome` as the report's P&L source.
    /// A closed trade maps to its exit kind (StoppedOut / TookProfit /
    /// ClosedOnReversal / ClosedAtExpiry); a still-open position → `Open` (no exit
    /// yet); a cancelled-or-absent order → `None` (no fill, exactly as the re-sim
    /// returned for a cancelled/unresolved order). The window-end anchor for an
    /// open position is the last pulled candle.
    pub fn held_realized_outcome(&self, order_id: &str) -> Option<RealizedOutcome> {
        // Advance to the window end so a position that closes on the last bars is
        // reflected. The loop already advanced per bar; this is a final settle.
        if let Some(last) = self.candles.last().map(|c| c.time) {
            self.advance(last);
        }
        if let Some(t) = self.closed.borrow().iter().find(|t| t.order_id == order_id) {
            let kind = match t.reason {
                ExitReason::StoppedOut => FillKind::StoppedOut,
                ExitReason::TookProfit => FillKind::TookProfit,
                ExitReason::Reversal => FillKind::ClosedOnReversal,
                ExitReason::Expiry => FillKind::ClosedAtExpiry,
                ExitReason::Invalidation => FillKind::ClosedOnInvalidation,
            };
            return Some(RealizedOutcome {
                direction: t.direction,
                fill_at: t.fill_at,
                until: t.exit_at,
                entry_price: t.entry_price,
                stop_loss: t.stop_loss,
                take_profit: t.take_profit,
                exit_price: Some(t.exit_price),
                kind,
            });
        }
        if let Some(p) = self.open.borrow().iter().find(|p| p.order_id == order_id) {
            let window_end = self.candles.last().map(|c| c.time).unwrap_or(p.fill_at);
            return Some(RealizedOutcome {
                direction: p.direction,
                fill_at: p.fill_at,
                until: window_end,
                entry_price: p.entry_price,
                stop_loss: p
                    .placed
                    .as_ref()
                    .map(|pl| pl.stop_loss)
                    .unwrap_or(p.entry_price),
                take_profit: p
                    .placed
                    .as_ref()
                    .map(|pl| pl.take_profit)
                    .unwrap_or(p.entry_price),
                exit_price: None,
                kind: FillKind::Open,
            });
        }
        // Still resting at window end. An UNCANCELLED resting order is a genuine
        // NeverFilled (the trigger was never reached) — distinct from a cancelled
        // one (spread-hour cancel / superseded), which is a true no-fill (`None`).
        // The report renders NeverFilled with its intended (unfilled) bracket
        // anchored at the fire bar; a `None` becomes the "order cancelled" no-fill.
        if let Some(o) = self
            .resting
            .borrow()
            .iter()
            .find(|o| o.order_id == order_id)
        {
            if o.cancelled {
                return None;
            }
            // Resolve the intended bracket for the not-taken box (fire-bar anchored).
            let probe = self.resolved_for_sim_probe(&o.intent, &o.shell, &o.placed);
            let window_end = self.candles.last().map(|c| c.time).unwrap_or(o.shell.time);
            if let Some(resolved) = probe {
                return Some(RealizedOutcome {
                    direction: resolved.direction,
                    fill_at: o.shell.time,
                    until: window_end,
                    entry_price: resolved.entry.reference_price(),
                    stop_loss: resolved.stop_loss,
                    take_profit: resolved.take_profit,
                    exit_price: None,
                    kind: FillKind::NeverFilled,
                });
            }
        }
        // Cancelled or never-placed — no fill (the report renders a 0R no-fill).
        None
    }

    /// Resolve a held order/position's bracket for a not-taken outcome box, off its
    /// intent+shell+placed levels (the same stored-levels-or-floor logic
    /// `step_outcome` uses). A thin wrapper so `held_realized_outcome` can anchor a
    /// `NeverFilled` box without an `advance` step.
    fn resolved_for_sim_probe(
        &self,
        intent: &Intent,
        shell: &Shell,
        placed: &Option<PlacedLevels>,
    ) -> Option<Resolved> {
        let probe = PlacedAttempt {
            order_id: String::new(),
            intent: intent.clone(),
            shell: shell.clone(),
            placed: placed.clone(),
            cancelled: false,
        };
        self.resolved_for_sim(&probe, &[])
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
                super::fill_sim::apply_entry_spread_floor(
                    &mut resolved,
                    self.pip_size,
                    forward,
                    None,
                );
            }
        }
        Some(resolved)
    }

    /// The prefix a held order/position is simulated against as of the current
    /// bar: the candles at/after its fire (`shell`) bar, up to and including
    /// `as_of`. Index 0 is the fire bar — the sim's `find_fill` excludes it (a
    /// resting order isn't live until its fire bar closes), so `advance()` gets
    /// the fire-bar skip (and the spread-hour fill skip, sub-bar zoom, break-even,
    /// System-2 widen) for free from `simulate_fill_resolved_zoom`.
    fn prefix_from_fire(&self, shell: &Shell, up_to: DateTime<Utc>) -> Vec<BidAskCandle> {
        // Bound at `up_to` (the current bar's OPEN time), inclusive — NOT the
        // shared `as_of`, which the loop sets to the bar CLOSE (`now`). Because
        // candle timestamps are bar-open times and a bar's close equals the NEXT
        // bar's open, using the close as the bound would pull the next bar's open
        // price into the fill test and fill an order a bar early (the divergence
        // the cancel-and-replace test exposed: a stop filled at bar N's advance
        // off bar N+1's open before the bar-N cancel could land). The re-sim
        // `resolve` avoids this because every dispatch-time lookup bounds at the
        // firing bar's OPEN (`fired.candle.time`); `advance` matches that.
        self.candles
            .iter()
            .filter(|c| c.time >= shell.time && c.time <= up_to)
            .cloned()
            .collect()
    }

    /// Simulate one held order/position against the prefix up to `as_of` and read
    /// off its state *by this bar* — the same `simulate_fill_resolved_zoom` the
    /// re-sim `resolve` uses, so `advance()` reproduces every fill/exit invariant
    /// baked into the engine. Returns `None` when the intent can't resolve (slot
    /// free) — the caller drops the order.
    fn step_outcome(
        &self,
        intent: &Intent,
        shell: &Shell,
        placed: &Option<PlacedLevels>,
        up_to: DateTime<Utc>,
    ) -> Option<(Resolved, SimOutcome)> {
        let prefix = self.prefix_from_fire(shell, up_to);
        // Build a throwaway attempt so `resolved_for_sim` (which reads
        // `attempt.placed` / `attempt.intent` / `attempt.shell`) applies the SAME
        // stored-levels-or-floor logic the re-sim path uses. No ledger/cancel
        // fields matter here — only the three the resolver reads.
        let probe = PlacedAttempt {
            order_id: String::new(),
            intent: intent.clone(),
            shell: shell.clone(),
            placed: placed.clone(),
            cancelled: false,
        };
        let resolved = self.resolved_for_sim(&probe, &prefix)?;
        let outcome = simulate_fill_resolved_zoom(
            &resolved,
            intent,
            shell,
            self.pip_size,
            &prefix,
            self.zoom(),
        );
        Some((resolved, outcome))
    }

    /// Advance the held state to `as_of` (call once per bar, AFTER `set_as_of`,
    /// BEFORE engine dispatch). This is the single-source-of-truth step that
    /// replaces the re-simulate-on-query model: it promotes resting→open on a
    /// fill and open→closed on an SL/TP touch, by this bar, so `list_open_positions`
    /// / `lookup_attempt_state` can READ held state instead of re-deriving it, and
    /// `close_positions` (reversal / expiry, dispatched by the engine on this bar)
    /// has a real position to flatten. Reuses `simulate_fill_resolved_zoom`, so
    /// every fill/exit invariant (fire-bar skip, spread-hour skip, sub-bar zoom,
    /// break-even, System-2 widen) is preserved — no reimplemented fill engine.
    pub fn advance(&self, up_to: DateTime<Utc>) {
        // 1. Resting → open (or straight to closed if it filled AND exited by now).
        //    A cancelled resting order fills nothing; leave it for the lifecycle.
        let resting_now = self.resting.borrow().clone();
        for order in resting_now {
            if order.cancelled {
                continue;
            }
            let Some((resolved, outcome)) =
                self.step_outcome(&order.intent, &order.shell, &order.placed, up_to)
            else {
                // Unresolvable → the slot is free; drop the resting order.
                self.remove_resting(&order.order_id);
                continue;
            };
            match outcome {
                SimOutcome::NeverFilled => { /* still resting */ }
                SimOutcome::FilledOpen {
                    fill_at,
                    entry_price,
                } => {
                    self.remove_resting(&order.order_id);
                    self.open.borrow_mut().push(HeldPosition {
                        order_id: order.order_id.clone(),
                        intent: order.intent.clone(),
                        shell: order.shell.clone(),
                        placed: order.placed.clone(),
                        direction: resolved.direction,
                        entry_price,
                        fill_at,
                    });
                }
                SimOutcome::StoppedOut {
                    fill_at,
                    entry_price,
                    exit_at,
                    exit_price,
                }
                | SimOutcome::TookProfit {
                    fill_at,
                    entry_price,
                    exit_at,
                    exit_price,
                } => {
                    // Filled AND exited within the prefix — record the closed trade
                    // directly (it never rests as "open" past this bar).
                    let reason = if matches!(outcome, SimOutcome::TookProfit { .. }) {
                        ExitReason::TookProfit
                    } else {
                        ExitReason::StoppedOut
                    };
                    self.remove_resting(&order.order_id);
                    self.closed.borrow_mut().push(ClosedTrade {
                        order_id: order.order_id.clone(),
                        direction: resolved.direction,
                        entry_price,
                        stop_loss: resolved.stop_loss,
                        take_profit: resolved.take_profit,
                        fill_at,
                        exit_at,
                        exit_price,
                        reason,
                    });
                }
                SimOutcome::Declined { .. } | SimOutcome::Unresolved(_) => {
                    self.remove_resting(&order.order_id);
                }
            }
        }

        // 2. Open → closed on an SL/TP touch by this bar. (Reversal / expiry
        //    closes are applied by the engine via `close_positions`, not here.)
        let open_now = self.open.borrow().clone();
        for pos in open_now {
            let Some((resolved, outcome)) =
                self.step_outcome(&pos.intent, &pos.shell, &pos.placed, up_to)
            else {
                continue;
            };
            if let SimOutcome::StoppedOut {
                exit_at,
                exit_price,
                ..
            }
            | SimOutcome::TookProfit {
                exit_at,
                exit_price,
                ..
            } = outcome
            {
                let reason = if matches!(outcome, SimOutcome::TookProfit { .. }) {
                    ExitReason::TookProfit
                } else {
                    ExitReason::StoppedOut
                };
                self.remove_open(&pos.order_id);
                self.closed.borrow_mut().push(ClosedTrade {
                    order_id: pos.order_id.clone(),
                    direction: pos.direction,
                    entry_price: pos.entry_price,
                    stop_loss: resolved.stop_loss,
                    take_profit: resolved.take_profit,
                    fill_at: pos.fill_at,
                    exit_at,
                    exit_price,
                    reason,
                });
            }
        }
    }

    /// The held-model `AttemptState` for an order id — the S4 read that replaces
    /// the re-sim `resolve` for the retry-gate. Mirrors `resolve`'s exact mapping:
    /// a resting order (uncancelled) → `Pending`; an open position → `OpenPosition`
    /// with the `{order_id}-pos` trade id; a closed trade → `ClosedWin` /
    /// `ClosedLossOrBreakeven` with the ±1.0 sentinel `realized_pl` the gate keys
    /// on; a cancelled or absent order → `Cancelled`. An id we never placed →
    /// `Unknown` (fail-safe). The categories are shadow-parity asserted vs
    /// `resolve` bar-by-bar through S3–S7.
    fn held_attempt_state(&self, order_id: &str) -> AttemptState {
        // Advance the held state to the current `as_of` first, so an isolated
        // caller (a unit test that sets `as_of` and reads, without the loop's
        // per-bar advance) sees the same progression the loop produces. In the
        // loop this is a no-op-or-forward: `advance` only ever promotes on a
        // genuine transition by `as_of`, never backward. Bounds at `as_of`, which
        // the caller set to the bar it's asking about.
        self.advance(*self.as_of.borrow());
        if let Some(o) = self
            .resting
            .borrow()
            .iter()
            .find(|o| o.order_id == order_id)
        {
            return if o.cancelled {
                AttemptState::Cancelled
            } else {
                AttemptState::Pending
            };
        }
        if self.open.borrow().iter().any(|p| p.order_id == order_id) {
            return AttemptState::OpenPosition {
                broker_trade_id: format!("{order_id}-pos"),
            };
        }
        if let Some(t) = self.closed.borrow().iter().find(|t| t.order_id == order_id) {
            return match t.reason {
                ExitReason::TookProfit => AttemptState::ClosedWin { realized_pl: 1.0 },
                // StoppedOut / Reversal / Expiry → loss-or-breakeven (re-sim maps
                // any non-TP close to ClosedLossOrBreakeven with -1.0).
                _ => AttemptState::ClosedLossOrBreakeven { realized_pl: -1.0 },
            };
        }
        // Never placed (or dropped as unresolvable) — the gate only asks about ids
        // it placed, so an unknown id is fail-safe `Unknown`; a dropped one reads
        // as `Cancelled` via the resting/open/closed miss above is impossible
        // (it's simply absent), so treat absent as `Unknown` to match the re-sim's
        // `None => Unknown` arm in `lookup_attempt_state`.
        AttemptState::Unknown
    }

    /// Remove a resting order by id (filled, cancelled-and-dropped, or unresolvable).
    fn remove_resting(&self, order_id: &str) {
        self.resting.borrow_mut().retain(|o| o.order_id != order_id);
    }

    /// Remove an open position by id (closed by bracket, reversal, or expiry).
    fn remove_open(&self, order_id: &str) {
        self.open.borrow_mut().retain(|p| p.order_id != order_id);
    }

    /// The held-order variant of [`pending_from_attempt`] (S7): same trigger/
    /// direction resolution, off a `HeldOrder`'s intent+shell. Keeps the
    /// `list_pending_orders` reconstruction reading held state.
    fn pending_from_held(&self, o: &HeldOrder) -> PendingOrder {
        use trade_control_core::intent::{Direction, Resolved, ResolvedEntry};
        let direction = o.intent.direction.unwrap_or(Direction::Long);
        let (trigger, is_stop) =
            match Resolved::from_intent(&o.intent, &o.shell, self.pip_size, self.pip_size) {
                Ok(r) => match r.entry {
                    ResolvedEntry::Stop { trigger_price } => (trigger_price, true),
                    ResolvedEntry::Limit { trigger_price } => (trigger_price, false),
                    ResolvedEntry::Market { reference_price } => (reference_price, true),
                },
                Err(_) => (o.shell.close, true),
            };
        PendingOrder {
            order_id: o.order_id.clone(),
            instrument: o.intent.instrument.clone(),
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
        // 2. Open-positions cap: count HELD open positions as-of the fire bar (S6 —
        //    the same held state `list_open_positions` reports) and reject at the
        //    cap, mirroring the real broker's `open_position_count >= cap`. In a
        //    single-plan replay this is that instrument's open count — the best
        //    offline proxy for the account-wide count, and conservative (it can
        //    only reject, never over-fill). Advance to the current `as_of` first so
        //    a fill/close that happened by this bar is reflected.
        self.advance(*self.as_of.borrow());
        let open_now = self.open.borrow().len();
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

    async fn close_positions(&self, instrument: &str) -> bool {
        // S5: actually flatten held open positions for this instrument at the
        // current bar's close — the live worker's `run_close` / ClosePositions
        // veto flattens at market when the engine dispatches the close, so the
        // bar's close is the faithful exit price. The loop sets the reason
        // (Reversal by default; Expiry for the trade-expiry veto) via
        // `set_close_reason` right before the engine dispatches this close.
        // Returns true iff at least one position was closed (mirrors the real
        // broker's "did I close anything").
        let Some(bar) = self.candle_at_as_of() else {
            return false;
        };
        let exit_at = bar.time;
        let exit_price = (bar.bid_c + bar.ask_c) / 2.0;
        let reason = *self.close_reason.borrow();
        let inst_key = instrument.to_lowercase();

        let mut to_close = Vec::new();
        self.open.borrow_mut().retain(|p| {
            if p.intent.instrument.to_lowercase() == inst_key {
                to_close.push(p.clone());
                false // remove from open
            } else {
                true
            }
        });
        if to_close.is_empty() {
            return false;
        }
        let mut closed = self.closed.borrow_mut();
        for p in to_close {
            closed.push(ClosedTrade {
                order_id: p.order_id,
                direction: p.direction,
                entry_price: p.entry_price,
                // Resolve the stored floored stop for R scoring; fall back to the
                // entry (0-risk → 0R) if the intent can't resolve (shouldn't happen
                // for an order that filled).
                stop_loss: p
                    .placed
                    .as_ref()
                    .map(|pl| pl.stop_loss)
                    .unwrap_or(p.entry_price),
                take_profit: p
                    .placed
                    .as_ref()
                    .map(|pl| pl.take_profit)
                    .unwrap_or(p.entry_price),
                fill_at: p.fill_at,
                exit_at,
                exit_price,
                reason,
            });
        }
        true
    }

    async fn cancel_pending_for_instrument(&self, instrument: &str) -> usize {
        // Cancel all held resting orders for this instrument (the ClosePositions
        // veto and reversal path also cancel pending orders live). Returns the
        // count cancelled, mirroring the real broker.
        let inst_key = instrument.to_lowercase();
        let mut n = 0;
        for o in self.resting.borrow_mut().iter_mut() {
            if !o.cancelled && o.intent.instrument.to_lowercase() == inst_key {
                o.cancelled = true;
                n += 1;
            }
        }
        n
    }

    async fn lookup_attempt_state(
        &self,
        _instrument: &str,
        broker_order_id: &str,
        _broker_trade_id: Option<&str>,
    ) -> Result<AttemptState, LookupError> {
        // S4: READ held state (advanced by the loop's per-bar `advance(bar_open)`)
        // instead of re-simulating. The held snapshot is current as-of the current
        // bar's open — the same instant the dispatch-time gate lookups bound at
        // (`fired.candle.time`). Categories mirror the re-sim's `resolve` exactly
        // (shadow-parity asserted through S3–S7). `close_positions` (S5) removes a
        // reversal/expiry-closed position from `open`, so this read then frees the
        // slot for re-entry — the fix that unblocks the EUR/USD case.
        Ok(self.held_attempt_state(broker_order_id))
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
        // Mirror onto the held resting order: a cancelled resting order fills
        // nothing (advance() skips `cancelled`) and appears in neither the open
        // nor the pending list — matching the re-sim's `Cancelled`. A restore
        // re-activates it (`reactivate_matching_cancelled`). If the order has
        // ALREADY been promoted to `open` by an earlier `advance()`, the cancel is
        // a no-op on it — which is correct: you can't cancel-pending a filled
        // order (the retry gate only cancels one it observed `Pending`, so this
        // path is reached only while it's still resting).
        if let Some(o) = self
            .resting
            .borrow_mut()
            .iter_mut()
            .find(|o| o.order_id == broker_order_id)
        {
            o.cancelled = true;
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
        // The Bug #11 backstop: report an open position for every HELD open
        // position (S4), keyed back to its order id so the gate's correlation
        // matches. Reads held state instead of re-simulating — so once
        // `close_positions` (S5) removes a reversal/expiry-closed position, the
        // backstop no longer reports it and re-entry is freed (the EUR/USD fix).
        self.advance(*self.as_of.borrow());
        let positions = self
            .open
            .borrow()
            .iter()
            .map(|p| OpenPosition {
                instrument: p.intent.instrument.clone(),
                direction: p.direction,
                stop_loss: None,
                take_profit: None,
                position_id: format!("{}-pos", p.order_id),
                order_id: p.order_id.clone(),
                stake: 1.0,
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
        // S7: report a resting order for every HELD resting order that is not
        // cancelled and not yet filled by `as_of`. This is what the shared
        // `pending_order_lifecycle` (core) lists to decide what to cancel through a
        // spread hour; a mock that always returned `[]` would make the lifecycle a
        // no-op offline, so replay could never reproduce the live cancel/restore.
        // Reads held state (advanced to `as_of`) instead of re-simulating.
        self.advance(*self.as_of.borrow());
        let pendings = self
            .resting
            .borrow()
            .iter()
            .filter(|o| !o.cancelled)
            .map(|o| self.pending_from_held(o))
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

    /// BUG-same-bar-fill-and-stop, broker half: with `as_of` at the placement
    /// bar's OPEN — the contract [`ReplayBroker::set_as_of`] documents — an order
    /// placed on bar N must NOT resolve against bar N+1, even when bar N+1
    /// straddles both its trigger and its stop.
    ///
    /// The bug was a caller passing the bar CLOSE instead (the replay loop's
    /// `now`, at the `pending_order_lifecycle` step). A close equals the next
    /// bar's open by value, so the broker cannot detect the mistake from the
    /// argument alone — the contract has to be honoured by the caller. What this
    /// test pins is the half the broker CAN guarantee: given a bar-open `as_of`,
    /// the fill window genuinely stops there and the next bar stays invisible.
    #[tokio::test]
    async fn as_of_at_bar_open_does_not_resolve_against_the_next_bar() {
        let bar = 3600;
        let fire_bar = candle(0, 1.1010); // above the 1.1000 sell-stop — no touch
        // Bar 1 spans BOTH the trigger and the stop — the ambiguity that turns a
        // one-bar peek into a fabricated fill-and-stop.
        let mut straddle = candle(bar, 1.1010);
        straddle.l = 1.0990;
        straddle.bid_l = 1.0990;
        straddle.h = 1.1030;
        straddle.ask_h = 1.1030;
        let b = ReplayBroker::new(vec![fire_bar, straddle], 0.0001);
        b.record_attempt(
            "o1".into(),
            short_enter_intent(),
            Shell::from_candle(&fire_bar.mid()),
            None,
        );

        b.set_as_of(Utc.timestamp_opt(0, 0).unwrap());
        let st = b.lookup_attempt_state("EUR/USD", "o1", None).await.unwrap();
        assert!(
            matches!(st, AttemptState::Pending),
            "as-of bar 0's OPEN the order has not filled, got {st:?}"
        );
        assert!(
            b.closed.borrow().is_empty(),
            "no trade may be closed before the loop reaches bar 1"
        );
    }
}
