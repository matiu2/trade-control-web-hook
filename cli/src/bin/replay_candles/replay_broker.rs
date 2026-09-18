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
//!
//! **A stub here must never LIE.** Where a method cannot do the real thing it
//! either reports what the simulator genuinely knows or fails loudly — it does
//! not return a cheerful success having changed nothing. `amend_stop` used to do
//! exactly that (ignore its id and level, return `Ok(())`) while
//! `list_open_positions` reported every position with no stop attached. Together
//! those two would let any live stop-management cron run offline, report success,
//! move nothing, and leave the entire fixture corpus green — a corpus that
//! appears to validate behaviour that never executed. See
//! `[[broker_adapter_stubs_are_lies]]`.

use std::cell::RefCell;

use super::fill_sim::{SimOutcome, simulate_fill_resolved_zoom};
use super::report::FillKind;
use chrono::{DateTime, Utc};
use trade_control_core::broker::{
    AmendError, AttemptState, BidAskCandle, Broker, CancelError, Candle, CandleError, CloseOutcome,
    EntryError, EntryRequest, Granularity, LookupError, OpenPosition, PendingOrder, Quote,
};
use trade_control_core::incoming::Verified;
use trade_control_core::intent::{Direction, Intent, Resolved, ResolvedEntry, RiskBudget, Shell};

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
    /// Placed by the order-control promote pass out of a gate park (a
    /// `SpreadHour` delay), not by the fire's own dispatch. Read by
    /// `promoted_order_id` so the replay can re-point that fire.
    promoted_from_park: bool,
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
    /// The stop level a [`Broker::amend_stop`] moved this RESTING order's SL to,
    /// or `None` while it still rests at its placed stop.
    ///
    /// Deliberately **beside** `placed` rather than overwriting it: `placed` is
    /// the "orders are state" record the fill simulator scores against
    /// (`resolved_for_sim`), and moving the scored stop is a different change
    /// with a different blast radius (audit findings #5/#8). This field records
    /// what the broker was *told*, so `list_open_positions` can report it back
    /// honestly; who *acts* on it is decided separately.
    amended_stop: Option<f64>,
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
    /// The stop level a [`Broker::amend_stop`] moved this position's SL to, or
    /// `None` while it still rests at its placed stop. This is what the live
    /// break-even watcher and the System-2 spread widen do to an open position.
    ///
    /// Deliberately **beside** `placed` rather than overwriting it — see
    /// [`HeldOrder::amended_stop`] for why. `list_open_positions` reports
    /// `amended_stop.or(placed stop)`, so a cron that amends and re-reads sees
    /// its own move, while the simulator keeps scoring the placed bracket.
    amended_stop: Option<f64>,
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

/// One sampled book the replay's `get_quote` answers from instead of the plan
/// bar `as_of` names: a finer bar's CLOSE book at an upkeep tick, or the OPEN
/// book of the finer bar opening at a plan-bar close (the entry-instant
/// sample — see `UpkeepTicks::opening_at`). Carries only what a quote is —
/// the instant and the two sides — so no caller can accidentally read a
/// candle's other fields through it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuoteSample {
    pub at: DateTime<Utc>,
    pub bid: f64,
    pub ask: f64,
}

impl QuoteSample {
    /// The bar's close book, stamped at its close (`at`).
    pub fn at_close(at: DateTime<Utc>, bar: &BidAskCandle) -> Self {
        Self {
            at,
            bid: bar.bid_c,
            ask: bar.ask_c,
        }
    }

    /// The bar's open book, stamped at its open.
    pub fn at_open(bar: &BidAskCandle) -> Self {
        Self {
            at: bar.time,
            bid: bar.bid_o,
            ask: bar.ask_o,
        }
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
    /// Enters the spread-blackout gate PARKED (H4+ delay, `StoredReason::
    /// SpreadHour`) instead of placing: the armed placement `run_enter` never
    /// consumed, kept so the park is recoverable by TRADE id
    /// (`armed_verified`) and so the promotion's `place_entry` — which arrives
    /// with nothing armed — can adopt its intent, shell and order id. Live has
    /// the signed body in the store for this; offline the armed map IS the body.
    parked: RefCell<Vec<ArmedPlacement>>,
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
    /// The sub-bar quote an **upkeep tick** is sampling (job 2: replay walks
    /// the live scheduler's 900 s order-control cadence). While set, `get_quote`
    /// answers from THIS bar's book and keys the spread-hour clamp on ITS time,
    /// instead of the plan bar `as_of` points at. Nothing else reads it: the
    /// held ledger, fills and `get_bidask_candles` stay bound to `as_of`, so an
    /// upkeep tick differs from the per-bar pass by exactly the clock and the
    /// quote — the operator's "shared code" constraint. `None` between ticks.
    upkeep_sample: RefCell<Option<QuoteSample>>,

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
            parked: RefCell::new(Vec::new()),
            finer: None,
            upkeep_sample: RefCell::new(None),
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

    /// Point `get_quote` at a finer bar's book for the duration of one upkeep
    /// tick (`Some`), or back at the plan bar `as_of` names (`None`). The held
    /// state is deliberately NOT moved: a sub-bar tick asks "what is the spread
    /// right now?" of the shared order-control passes, and nothing else.
    pub fn set_upkeep_sample(&self, sample: Option<QuoteSample>) {
        *self.upkeep_sample.borrow_mut() = sample;
    }

    /// Arm the placement for the next `run_enter`: the order id `place_entry`
    /// should return and the intent + shell needed to resolve this attempt's
    /// later state. Call right before dispatching the enter; `place_entry`
    /// consumes it. `order_id` must match what the gate stores on the
    /// `EntryAttempt` (`run_enter` stamps `place_entry`'s return there), so the
    /// minted id is the standard `{intent.id}-{attempt_no}` form.
    /// The gate parked the enter it was armed for (a `SpreadHour` delay): move
    /// the un-consumed arm to the parked set so the park can be recovered and
    /// later promoted. A no-op when nothing is armed (the arm was consumed by a
    /// placement, or this reject wasn't a park).
    pub fn park_armed(&self) {
        if let Some(a) = self.armed.borrow_mut().take() {
            self.parked.borrow_mut().push(a);
        }
    }

    /// The broker order id a PROMOTED park was placed under, so the replay can
    /// re-point the enter fire the gate rejected at the placement it later
    /// became. `None` while still parked (or never parked).
    pub fn promoted_order_id(&self, trade_id: &str) -> Option<String> {
        self.placed
            .borrow()
            .iter()
            .rev()
            .find(|a| a.promoted_from_park && a.intent.trade_id.as_deref() == Some(trade_id))
            .map(|a| a.order_id.clone())
    }

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
            // Rests at its placed stop until a cron amends it.
            amended_stop: None,
        });
        self.placed.borrow_mut().push(PlacedAttempt {
            order_id,
            intent,
            shell,
            placed,
            cancelled: false,
            promoted_from_park: false,
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
    /// `None` only for an **unknown** key — a cancelled order still exposes
    /// its armed Verified, because the lifecycle's restore side re-drives it
    /// *after* the cancel (the cancel flag gates the fill outcome, not the payload
    /// seam).
    ///
    /// # `key` is an order id OR a trade id
    ///
    /// The resting-order paths (`pending_order_lifecycle`, the re-price cancel)
    /// hold a broker **order id**. A **parked** order has no broker id at all — it
    /// was never placed — so `promote_stored_order` keys its recovery on the
    /// **trade id** instead, as that module's docs state. One seam serves both
    /// callers, so both are accepted; the order id is tried first because it is
    /// the exact identity and the overwhelmingly common case.
    ///
    /// Without the trade-id arm a park is **unpromotable offline**: it verifies on
    /// the way down (the cancel holds an order id) and fails on the way back up
    /// with `will not verify`, so a demoted order stays parked forever and the
    /// setup silently vanishes from the replay. That was latent until the
    /// re-price pass started producing parks offline — nothing else offline
    /// demotes.
    pub fn armed_verified(&self, key: &str) -> Option<Verified> {
        let placed = self.placed.borrow();
        let parked = self.parked.borrow();
        let (intent, shell) = placed
            .iter()
            .find(|a| a.order_id == key)
            .or_else(|| {
                placed
                    .iter()
                    .find(|a| a.intent.trade_id.as_deref() == Some(key))
            })
            .map(|a| (&a.intent, &a.shell))
            // A gate-parked enter was never placed, so it lives only here —
            // the trade-id arm is the ONLY way a `SpreadHour` park promotes.
            .or_else(|| {
                parked
                    .iter()
                    .find(|a| a.intent.trade_id.as_deref() == Some(key))
                    .map(|a| (&a.intent, &a.shell))
            })?;
        let mut intent = intent.clone();
        if !intent.pip_size.is_some_and(|p| p > 0.0 && p.is_finite()) {
            intent.pip_size = Some(self.pip_size);
        }
        Some(Verified {
            shell: shell.clone(),
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
    /// Remove and return the parked arm whose intent resolves to `req`'s
    /// instrument + direction — the promotion of a gate-parked enter. One plan
    /// per replay, so instrument + direction identify it.
    fn take_parked_matching(&self, req: &EntryRequest<'_>) -> Option<ArmedPlacement> {
        let mut parked = self.parked.borrow_mut();
        let idx = parked.iter().position(|a| {
            if a.intent.instrument != req.instrument {
                return false;
            }
            let tick = a.intent.tick_size.unwrap_or(self.pip_size);
            Resolved::from_intent(&a.intent, &a.shell, self.pip_size, tick)
                .is_ok_and(|r| r.direction == req.direction)
        })?;
        Some(parked.remove(idx))
    }

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
            promoted_from_park: false,
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
                    // The same tick `Resolved::from_intent` snapped the drawn
                    // geometry with above, so the floor's widened stop lands on
                    // the same grid as the entry and TP beside it.
                    tick,
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
            promoted_from_park: false,
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
                        // An amend that landed while the order was still resting
                        // carries onto the filled position — the real broker
                        // attaches the order's SL to the trade it opens, so
                        // dropping it here would make the amend silently expire at
                        // the fill.
                        amended_stop: order.amended_stop,
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

    /// Does `id` address this held position? Accepts BOTH spellings the broker
    /// itself hands out: the bare `order_id`, and the `{order_id}-pos`
    /// `position_id` reported by [`Broker::list_open_positions`].
    ///
    /// Both are needed because the live callers disagree on which they hold:
    /// `breakeven_watch` and `blackout_apply` amend by `position.order_id`, while
    /// the blackout *restore* pass matches a remembered stop on `order_id` **or**
    /// `position_id` (`blackout_watch.rs`). Accepting only one spelling is the
    /// id-mismatch class that has already cost this series a stranded park
    /// (recovered by `trade_id` while every resting path used `order_id`).
    fn position_addressed_by(pos: &HeldPosition, id: &str) -> bool {
        pos.order_id == id || format!("{}-pos", pos.order_id) == id
    }

    /// The stop level to REPORT for a held position: the amended level if a cron
    /// moved it, else the placed stop it rests at.
    ///
    /// Returns `None` only when the intent cannot resolve at all — i.e. we
    /// genuinely do not know the bracket, which is the one honest use of `None`
    /// here. It must never be the blanket answer: `None` reads to every live
    /// stop-management cron as "no stop attached", and all three of them
    /// (`breakeven_watch`, `blackout_apply`, `blackout_watch`) silently
    /// early-return on it.
    fn reported_bracket(&self, pos: &HeldPosition) -> (Option<f64>, Option<f64>) {
        let resolved = self.resolved_for_sim_probe(&pos.intent, &pos.shell, &pos.placed);
        let placed_stop = resolved.as_ref().map(|r| r.stop_loss);
        let take_profit = resolved.as_ref().map(|r| r.take_profit);
        (pos.amended_stop.or(placed_stop), take_profit)
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

/// The [`Placement`](trade_control_core::broker::Placement) an offline replay
/// can honestly report.
///
/// **`size` is always `None`.** Sizing needs live account equity + an FX rate,
/// which the offline replay has by definition not got — the real brokers
/// compute it from an account fetch. Reporting a guessed size here would put a
/// number in the report that never reached any book, so the replay says
/// "unknown" instead. (This is the same reason `place_entry` only enforces the
/// `Percent` risk cap offline and leaves `Amount`/`Units` unchecked.)
///
/// `price` IS known — it is the requested entry the replay is placing at, the
/// same quantity the live brokers report. Not a fill.
fn replay_placement(
    order_id: String,
    req: &EntryRequest<'_>,
) -> trade_control_core::broker::Placement {
    trade_control_core::broker::Placement {
        order_id,
        size: None,
        price: Some(req.entry.reference_price()),
    }
}

impl Broker for ReplayBroker {
    async fn place_entry(
        &self,
        max_risk_pct: f64,
        max_open_positions: u32,
        req: &EntryRequest<'_>,
    ) -> Result<trade_control_core::broker::Placement, EntryError> {
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
        //
        //    ⚠️ This is an ACCOUNT-WIDE BACKSTOP, not entry dedup, and it must
        //    not be mistaken for one. Until 2026-09 it was the only reason a
        //    replayed M/W plan stopped at one entry — the enter skipped the
        //    retry gate entirely, so the duplicates were suppressed here by a
        //    cap rather than refused by a gate. That MASKED the live bug (three
        //    simultaneous EUR/GBP positions, 2026-08-20) rather than agreeing
        //    with live. Per-plan dedup is `retry_gate::evaluate`, reached via
        //    `Intent::entry_dedup`; see
        //    `BUG-mw-everybar-enter-skips-retry-gate.md`. A green fixture
        //    corpus is not evidence about entry dedup.
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
                Ok(replay_placement(a.order_id, req))
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
                Some(order_id) => Ok(replay_placement(order_id, req)),
                // …or the order-control promote pass placing a gate-PARKED
                // enter (`StoredReason::SpreadHour`): adopt the arm the gate
                // set aside, under the id the fire would have placed with, and
                // hold it at the levels `run_enter` just derived off the calm
                // spread — the fresh floor and size the promotion is for.
                None if let Some(a) = self.take_parked_matching(req) => {
                    let placed = PlacedLevels {
                        entry: req.entry.clone(),
                        stop_loss: req.stop_loss,
                        take_profit: req.take_profit,
                    };
                    // The order goes live NOW, not on the bar the enter fired:
                    // `prefix_from_fire` opens the fill window at `shell.time`,
                    // so an adopted fire-bar shell would let a park promoted at
                    // 01:00Z fill on the 21:00Z bar that closed BEFORE it — a
                    // look-ahead. Stamp the placement at the broker's clock:
                    // the bar being processed (an upkeep tick inside bar N
                    // still sees `as_of` = N−1, so it stays fillable from N).
                    let mut shell = a.shell;
                    shell.time = *self.as_of.borrow();
                    self.record_attempt(a.order_id.clone(), a.intent, shell, Some(placed));
                    if let Some(rec) = self.placed.borrow_mut().last_mut() {
                        rec.promoted_from_park = true;
                    }
                    Ok(replay_placement(a.order_id, req))
                }
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

    async fn close_positions(&self, instrument: &str) -> CloseOutcome {
        // S5: actually flatten held open positions for this instrument at the
        // current bar's close — the live worker's `run_close` / ClosePositions
        // veto flattens at market when the engine dispatches the close, so the
        // bar's close is the faithful exit price. The loop sets the reason
        // (Reversal by default; Expiry for the trade-expiry veto) via
        // `set_close_reason` right before the engine dispatches this close.
        // Mirrors the real broker's three-way `CloseOutcome`: no bar to price
        // the exit against is the replay analogue of a broker call we cannot
        // complete (`Errored`), an instrument with nothing held is a
        // successful no-op (`NothingOpen`), and anything else reports the
        // count closed. Keeping the same three cases here is what keeps
        // replay == live for the `close-failed` distinction.
        let Some(bar) = self.candle_at_as_of() else {
            return CloseOutcome::Errored;
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
            return CloseOutcome::NothingOpen;
        }
        let closed_count = to_close.len();
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
        CloseOutcome::Closed(closed_count)
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
        // An upkeep tick (job 2) samples a finer bar's CLOSE book at that bar's
        // close, and the per-bar pass samples the OPEN book of the finer bar
        // opening at the plan-bar close (the entry-instant quote); with no
        // sample set, the plan bar `as_of` points at answers.
        if let Some(s) = *self.upkeep_sample.borrow() {
            return Ok(Quote {
                bid: s.bid,
                ask: s.ask,
            });
        }
        let book = self.candle_at_as_of().cloned();
        // No spread-hour clamp any more. Until 2026-09-18 an in-hour quote was
        // pinned to EXACTLY `elevated_threshold_pips`, which (a) made the
        // entry gate's strict `>` inert offline — the replay ENTERED on every
        // rollover bar live rejects (audit 2026-09-13, finding #10) — and (b)
        // kept the lifecycle OFF side from ping-ponging on a narrow close
        // print. (b) is now moot: the OFF rule is the shared
        // `spread_hour_released_at`, and with `--upkeep` the replay samples the
        // same sub-bar quotes live does, so an early release off a genuinely
        // calm sub-bar is the LIVE behaviour, not noise. The real book flows
        // through; the gate rejects (H1-) or parks (H4+) where live would.
        match book {
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
            .map(|p| {
                // Report the REAL bracket: the stop this position rests on (the
                // placed levels, or whatever a later `amend_stop` moved it to) and
                // its target. Hard-coding `None` here — as this did — is the same
                // class of lie as an empty position list: all three live
                // stop-management crons read `None` as "no stop attached" and
                // silently skip the position, so wiring any of them to this broker
                // would produce a fully green, entirely vacuous run. Held to the
                // same standard as `entry_price`/`opened_at` below, and for the
                // same reason.
                let (stop_loss, take_profit) = self.reported_bracket(p);
                OpenPosition {
                    instrument: p.intent.instrument.clone(),
                    direction: p.direction,
                    stop_loss,
                    take_profit,
                    position_id: format!("{}-pos", p.order_id),
                    order_id: p.order_id.clone(),
                    stake: 1.0,
                    // The simulator knows the true fill — report it, so anything
                    // that bounds on the fill (the break-even watcher) sees the
                    // same facts here as it does off a live broker.
                    entry_price: Some(p.entry_price),
                    opened_at: Some(p.fill_at),
                }
            })
            .collect();
        Ok(positions)
    }

    /// Record a stop move against the held order/position `position_or_order_id`
    /// addresses, mirroring the trait's stated matching order: open positions
    /// first, then resting orders.
    ///
    /// **Why this is not a stub.** It used to underscore-ignore both arguments and
    /// return `Ok(())`. Nothing in replay manages stops *today*, so that was not an
    /// active bug — but it made an entire class of wiring bug undetectable: point
    /// any live stop-management cron (`breakeven_watch`, `blackout_apply`'s
    /// System-2 widen, the blackout restore) at this broker and it would RUN,
    /// REPORT SUCCESS, MOVE NOTHING, and leave the whole fixture corpus green. A
    /// corpus that appears to validate stop management which never executed is
    /// worse than one that fails loudly. See `[[broker_adapter_stubs_are_lies]]`.
    ///
    /// Hence both halves: the amend is **recorded** (so a caller that re-reads
    /// sees its own move) and an id we do not hold is **rejected** with
    /// [`AmendError::NotFound`] — the variant the trait documents for an unmatched
    /// id, and the one the live crons already treat as benign-but-logged. An amend
    /// against an id that never existed is exactly the wiring bug the old `Ok(())`
    /// concealed.
    ///
    /// The recorded level deliberately does **not** move what the fill simulator
    /// scores (that walks the stored [`PlacedLevels`], untouched here). This
    /// method makes the broker tell the truth; which subsystem *acts* on the
    /// amended stop is a separate decision — see audit findings #5/#8.
    async fn amend_stop(
        &self,
        _account_id: &str,
        position_or_order_id: &str,
        new_stop: f64,
    ) -> Result<(), AmendError> {
        // Advance the held state to `as_of` first, exactly as every other held-state
        // reader does (`list_open_positions`, `list_pending_orders`,
        // `held_attempt_state`). Without it an order that has already FILLED by this
        // bar but not yet been advanced is still sitting in `resting`, so the amend
        // would land on the stale resting record while `list_open_positions` — which
        // does advance — reports the position and reads back the unamended stop. The
        // amend would appear to succeed and then vanish.
        self.advance(*self.as_of.borrow());
        // Open positions first (the trait's documented matching order, and what
        // every production caller of this method actually holds — all three amend
        // an OpenPosition they just listed).
        if let Some(pos) = self
            .open
            .borrow_mut()
            .iter_mut()
            .find(|p| Self::position_addressed_by(p, position_or_order_id))
        {
            tracing::debug!(
                "ReplayBroker::amend_stop: position {position_or_order_id} stop -> {new_stop}"
            );
            pos.amended_stop = Some(new_stop);
            return Ok(());
        }
        // Then resting orders — a pending entry's SL can also be amended. A
        // cancelled order is not amendable: it rests on no book, so an amend
        // against it is as much a wiring bug as an unknown id.
        if let Some(order) = self
            .resting
            .borrow_mut()
            .iter_mut()
            .find(|o| o.order_id == position_or_order_id && !o.cancelled)
        {
            tracing::debug!(
                "ReplayBroker::amend_stop: resting order {position_or_order_id} stop -> {new_stop}"
            );
            order.amended_stop = Some(new_stop);
            return Ok(());
        }
        tracing::error!(
            "ReplayBroker::amend_stop: no open position or resting order with id \
             {position_or_order_id} (asked to move its stop to {new_stop}) — reporting NotFound \
             rather than a silent Ok, which would hide the wiring bug"
        );
        Err(AmendError::NotFound)
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

    /// Job 2 (replay walks the upkeep ticks): while an upkeep sample is set,
    /// `get_quote` answers from THAT sub-bar's book, not the plan bar `as_of`
    /// names; clearing it restores the plan-bar quote. Neither call moves
    /// `as_of` — the held ledger is untouched by a sample.
    #[tokio::test]
    async fn upkeep_sample_overrides_the_as_of_bar_quote_until_cleared() {
        let wide = spread_candle(0, 1.10000, 1.10050); // 5.0 pip D1 rollover close
        let b = ReplayBroker::new(vec![wide], 0.0001);
        b.set_as_of(Utc.timestamp_opt(0, 0).unwrap());

        // A mid-bar H1 sample with a calm 0.3 pip spread.
        b.set_upkeep_sample(Some(QuoteSample::at_close(
            Utc.timestamp_opt(10800, 0).unwrap(),
            &spread_candle(7200, 1.10100, 1.10103),
        )));
        let q = b.get_quote("EUR/USD").await.unwrap();
        assert_eq!((q.bid, q.ask), (1.10100, 1.10103));
        assert!((q.spread() / 0.0001 - 0.3).abs() < 1e-9, "sample spread");
        assert_eq!(
            *b.as_of.borrow(),
            Utc.timestamp_opt(0, 0).unwrap(),
            "a sample never moves the held clock"
        );

        b.set_upkeep_sample(None);
        let q = b.get_quote("EUR/USD").await.unwrap();
        assert_eq!((q.bid, q.ask), (1.10000, 1.10050), "back to the plan bar");
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
            // Replay does not size (it reports `size: None` by design), so a
            // multiplier would have nothing to multiply — see the accepted
            // replay sizing gap in the IBKR plan.
            contract_multiplier: None,
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
        assert_eq!(
            ok.unwrap().order_id,
            "o1",
            "1% at a 1% cap is allowed (not >)"
        );
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
        assert_eq!(
            ok.unwrap().order_id,
            "o2",
            "one open under a cap of 3 is allowed"
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

    /// A parked order is recovered by its **trade id**, every resting-order path
    /// by its **order id**, and one seam answers both. The regression this pins:
    /// with only the order-id arm, `promote_stored_order` gets `will not verify`
    /// and a demoted order is stranded parked forever — the setup silently
    /// disappears from the replay.
    ///
    /// Mutation check: drop the `trade_id` fallback and the second assertion goes
    /// red while the first stays green, which is exactly how the bug hid.
    #[tokio::test]
    async fn armed_verified_resolves_by_trade_id_as_well_as_order_id() {
        let b = ReplayBroker::new(vec![candle(0, 1.1010)], 0.0001);
        let shell = Shell::from_candle(&candle(0, 1.1010).mid());
        b.arm_placement("o1".into(), short_enter_intent(), shell);
        b.place_entry(1.0, 3, &entry_req(RiskBudget::Percent(0.5)))
            .await
            .expect("placed");

        assert!(
            b.armed_verified("o1").is_some(),
            "the order-id arm is what the lifecycle and the re-price cancel use",
        );
        assert!(
            b.armed_verified("t").is_some(),
            "…and the trade-id arm is what a PARK's promotion uses — without it a \
             demoted order can never be re-placed",
        );
        assert!(
            b.armed_verified("nope").is_none(),
            "an unknown key must still resolve to nothing, not to some other trade",
        );
    }

    // --- finding #9: the broker must TELL THE TRUTH about stops ---
    //
    // `amend_stop` used to underscore-ignore both its id and its level and return
    // `Ok(())`, and `list_open_positions` hard-coded `stop_loss: None` /
    // `take_profit: None`. That pair is worse than an unimplemented stub: wire any
    // live stop-management cron (`breakeven_watch`, `blackout_apply`'s System-2
    // widen, the restore) to this broker and it RUNS, REPORTS SUCCESS, MOVES
    // NOTHING, and leaves every fixture green — a corpus that appears to validate
    // stop management that never executed. See
    // `[[broker_adapter_stubs_are_lies]]`. These tests pin all three halves of the
    // honesty: report the placed stop, record an amend, and reject an id we do not
    // hold.
    //
    // NOTE these exercise the BROKER's reporting only. What the fill simulator
    // scores against is deliberately untouched (it has its own `active_stop`
    // model) — that is findings #5/#8's job, not this change's.

    /// Drive an order to a filled, held position at `as_of` = bar 1, returning the
    /// broker. Short stop-entry at 1.1000, SL 1.1020, TP 1.0950 (from
    /// [`short_enter_intent`]), placed through `place_entry` so the position
    /// carries real [`PlacedLevels`] rather than the legacy re-derive path.
    async fn broker_with_open_position() -> ReplayBroker {
        let fire = candle(0, 1.1010); // above the sell-stop: no fill on the fire bar
        let fill = candle(3600, 1.1000); // bid reaches the 1.1000 sell-stop → fills
        let b = ReplayBroker::new(vec![fire, fill], 0.0001);
        b.arm_placement(
            "o1".into(),
            short_enter_intent(),
            Shell::from_candle(&fire.mid()),
        );
        b.place_entry(1.0, 3, &entry_req(RiskBudget::Percent(0.5)))
            .await
            .expect("placed");
        b.set_as_of(Utc.timestamp_opt(3600, 0).unwrap());
        b
    }

    /// The placed bracket must come back on the open position, exactly as it does
    /// off a live broker. Before this, every replayed position reported
    /// `stop_loss: None` — which `breakeven_watch:197`, `blackout_apply:235` and
    /// `blackout_watch:235` each read as "no stop attached" and silently
    /// early-return from, so all three crons would no-op offline while looking
    /// perfectly healthy.
    #[tokio::test]
    async fn open_position_reports_its_placed_stop_and_target() {
        let b = broker_with_open_position().await;
        let positions = b.list_open_positions("").await.unwrap();
        assert_eq!(positions.len(), 1, "the order filled on bar 1");
        let p = &positions[0];
        assert_eq!(
            p.stop_loss,
            Some(1.1020),
            "the position rests on the PLACED stop — reporting None makes every \
             live stop-management cron silently skip it"
        );
        assert_eq!(
            p.take_profit,
            Some(1.0950),
            "the take-profit is known for exactly the same reason the stop is"
        );
    }

    /// An amend against a held position must be RECORDED, and the next read must
    /// show the moved stop. This is the break-even / widen round-trip: amend, then
    /// re-list and see your own move. A stub returning `Ok(())` passes the amend
    /// and fails this read-back — which is precisely the undetectable-wiring-bug
    /// shape being closed.
    #[tokio::test]
    async fn amend_stop_moves_the_reported_stop() {
        let b = broker_with_open_position().await;
        // Break-even on a short filled at ~1.1000: move the 1.1020 stop down to entry.
        b.amend_stop("", "o1", 1.1000)
            .await
            .expect("amending a held position must succeed");

        let positions = b.list_open_positions("").await.unwrap();
        assert_eq!(
            positions[0].stop_loss,
            Some(1.1000),
            "the amended stop must be what the broker reports back — an amend that \
             reports success but changes nothing is the lie this fixes"
        );
        assert_eq!(
            positions[0].take_profit,
            Some(1.0950),
            "amend_stop moves the STOP only; the take-profit is left untouched"
        );
    }

    /// The last amend wins, and it is still reported after the position is
    /// re-advanced (the amend lives on the held record, not on a transient).
    #[tokio::test]
    async fn the_latest_amend_is_the_one_reported() {
        let b = broker_with_open_position().await;
        b.amend_stop("", "o1", 1.1010).await.expect("first amend");
        b.amend_stop("", "o1", 1.1000).await.expect("second amend");
        let positions = b.list_open_positions("").await.unwrap();
        assert_eq!(
            positions[0].stop_loss,
            Some(1.1000),
            "a second amend supersedes the first"
        );
    }

    /// `blackout_watch` matches a remembered stop by `order_id` **or**
    /// `position_id` (`blackout_watch.rs:224`), so the broker must accept the
    /// `-pos` form it hands out in `list_open_positions` too. Accepting only one
    /// spelling is the id-mismatch class that has already bitten this series (a
    /// park recovered by `trade_id` while every resting path used `order_id`).
    #[tokio::test]
    async fn amend_accepts_the_position_id_form_as_well_as_the_order_id() {
        let b = broker_with_open_position().await;
        let position_id = b.list_open_positions("").await.unwrap()[0]
            .position_id
            .clone();
        assert_eq!(
            position_id, "o1-pos",
            "the id form the broker itself reports"
        );
        b.amend_stop("", &position_id, 1.1005)
            .await
            .expect("the position_id form must be accepted");
        assert_eq!(
            b.list_open_positions("").await.unwrap()[0].stop_loss,
            Some(1.1005),
            "…and it must move the same position"
        );
    }

    /// An amend against an id the broker does not hold is a WIRING BUG, and must
    /// surface as one. Returning `Ok(())` here is what let the old stub hide a
    /// cron amending an id that never existed — the failure mode that makes a
    /// green corpus meaningless. `AmendError::NotFound` is the honest variant and
    /// the one the trait documents ("an unmatched id yields NotFound"); the live
    /// crons already handle it as benign-but-logged.
    #[tokio::test]
    async fn amend_against_an_unknown_id_is_not_found() {
        let b = broker_with_open_position().await;
        let err = b
            .amend_stop("", "never-placed", 1.1000)
            .await
            .expect_err("an id we do not hold must NOT report success");
        assert_eq!(err, AmendError::NotFound);
    }

    /// A resting (unfilled) order's SL can also be amended — the trait matches
    /// open positions first, then pending orders. Pins that the resting arm is
    /// wired rather than falling through to `NotFound`.
    #[tokio::test]
    async fn amend_reaches_a_still_resting_order() {
        let fire = candle(0, 1.1010); // no fill: price never reaches the sell-stop
        let b = ReplayBroker::new(vec![fire, candle(3600, 1.1012)], 0.0001);
        b.arm_placement(
            "o1".into(),
            short_enter_intent(),
            Shell::from_candle(&fire.mid()),
        );
        b.place_entry(1.0, 3, &entry_req(RiskBudget::Percent(0.5)))
            .await
            .expect("placed");
        b.set_as_of(Utc.timestamp_opt(3600, 0).unwrap());
        assert_eq!(
            b.list_open_positions("").await.unwrap().len(),
            0,
            "precondition: nothing filled, so this can only match the resting arm"
        );
        b.amend_stop("", "o1", 1.1030)
            .await
            .expect("a resting order's stop must be amendable");
    }

    /// An amend that arrives BEFORE anything has advanced the held state must
    /// still land on the position, not on the stale resting record it was
    /// promoted from.
    ///
    /// The hazard: `advance()` moves a filled order out of `resting` into `open`,
    /// but only the readers that call it see that. `amend_stop` is a WRITER — if
    /// it skipped the advance, an amend arriving on the fill bar before any read
    /// would set `amended_stop` on the resting record, and the very next
    /// `list_open_positions` (which does advance) would promote a *fresh*
    /// position and report the unamended stop. The amend would report success and
    /// then evaporate — the exact silent-no-op shape this whole change exists to
    /// remove.
    #[tokio::test]
    async fn an_amend_before_any_read_still_lands_on_the_filled_position() {
        let fire = candle(0, 1.1010);
        let fill = candle(3600, 1.1000);
        let b = ReplayBroker::new(vec![fire, fill], 0.0001);
        b.arm_placement(
            "o1".into(),
            short_enter_intent(),
            Shell::from_candle(&fire.mid()),
        );
        b.place_entry(1.0, 3, &entry_req(RiskBudget::Percent(0.5)))
            .await
            .expect("placed");
        // Move to the fill bar but do NOT read anything — the held state still has
        // the order in `resting`.
        b.set_as_of(Utc.timestamp_opt(3600, 0).unwrap());
        // Address it by the POSITION id form — the spelling `blackout_watch`
        // may hold. Only the position arm answers to `o1-pos`, so without the
        // advance the order is still `resting` (which matches on the bare
        // `order_id` only) and the amend is rejected NotFound outright.
        b.amend_stop("", "o1-pos", 1.1000)
            .await
            .expect("the position-id form must resolve once the fill has advanced");

        let positions = b.list_open_positions("").await.unwrap();
        assert_eq!(positions.len(), 1, "the order filled on this bar");
        assert_eq!(
            positions[0].stop_loss,
            Some(1.1000),
            "the amend must survive the promotion — landing it on the stale resting \
             record would make it silently vanish at the next read"
        );
    }

    /// The amend must NOT move what the simulator scores. `placed` is the
    /// "orders are state" record every fill/exit test walks; the amended stop is
    /// recorded beside it. This is the scope line for finding #9: the broker now
    /// tells the truth about stops, and a SEPARATE change (#5/#8) decides who
    /// listens. If this test ever goes red, the scoring path has been touched and
    /// the corpus will move.
    #[tokio::test]
    async fn amend_does_not_move_the_stop_the_simulator_scores() {
        let b = broker_with_open_position().await;
        b.amend_stop("", "o1", 1.1000).await.expect("amended");
        // Read the held record itself, not the reported view. `list_open_positions`
        // both advances the held state and is the thing under test elsewhere; here
        // we want the underlying `placed` bracket the simulator walks.
        assert_eq!(b.list_open_positions("").await.unwrap().len(), 1);
        let pos = b.open.borrow()[0].clone();
        let placed = pos.placed.as_ref().expect("placed via place_entry");
        assert_eq!(
            placed.stop_loss, 1.1020,
            "the PLACED bracket the simulator scores must be untouched by an amend"
        );
        let resolved = b
            .resolved_for_sim_probe(&pos.intent, &pos.shell, &pos.placed)
            .expect("resolves");
        assert_eq!(
            resolved.stop_loss, 1.1020,
            "…and so must the Resolved the fill sim walks"
        );
    }
    /// #11 (audit): on an H4 bar spanning a spread hour the two spread-hour
    /// gates give DIFFERENT answers, and that is CORRECT — they answer
    /// different questions:
    ///
    /// * `suppress_on_spread_hour` — "is this candle's OHLC rubbish?" — is
    ///   gated at `bar_seconds <= 3600`, so on H4 it is **false**. One bad hour
    ///   inside a four-hour bar is diluted by three hours of genuine trading;
    ///   discarding the bar would throw away real data.
    /// * `is_spread_hour` — "should a resting order be pulled?" — is **not**
    ///   bar-gated, so on H4 it is **true**. A four-hour bar does not protect
    ///   an order from filling at 21:15, so it comes off the broker.
    ///
    /// Verified for the instant this test uses: at 2026-06-15T21:00Z on EUR/USD
    /// with `bar_seconds = 14400`, `suppress_on_spread_hour_bar_seconds` is
    /// `false` while `is_spread_hour` is `true` — asserted below so the test
    /// fails loudly if a mask regen ever moves the hour out from under it,
    /// rather than passing vacuously against a non-spread-hour bar.
    ///
    /// The audit read that pair as a RACE: "the lifecycle cancels while
    /// `find_fill` would fill on the same bar, so the outcome depends on
    /// interleave order." **This test is the refutation.** A cancelled order
    /// fills nothing whatever the fill simulator thinks of the bar, because
    /// `advance()` skips `cancelled` before ever consulting it. There is no
    /// interleave that opens a position from an order the lifecycle pulled.
    ///
    /// The guard is a single `continue` in `advance()`. Remove it and an order
    /// deliberately pulled would fill anyway — a position the live worker never
    /// takes, booked into the corpus as real R. That is what this pins.
    #[tokio::test]
    async fn a_cancelled_order_never_fills_even_on_an_unsuppressed_h4_spread_hour_bar() {
        // H4 spacing (14400s). The fire bar sits above the 1.1000 short-stop
        // trigger so it cannot fill there; the next bar's bid reaches through it.
        let fire = candle(1_781_542_800, 1.1010);
        let fill = candle(1_781_557_200, 1.1000);

        // The premise, asserted rather than assumed.
        assert!(
            !trade_control_core::spread_blackout::suppress_on_spread_hour_bar_seconds(
                "EUR/USD", fill.time, 14_400,
            ),
            "premise: an H4 spread-hour bar is NOT suppressed (the simulator will fill on it)",
        );
        assert!(
            trade_control_core::spread_blackout::is_spread_hour("EUR/USD", fill.time),
            "premise: the same instant IS a spread hour (the lifecycle will pull the order)",
        );

        // Control: left alone, this order DOES fill on that bar. Without this
        // the test could pass for the wrong reason — an order that never fills
        // anyway proves nothing about the cancel.
        let control = ReplayBroker::new(vec![fire, fill], 0.0001);
        control.arm_placement(
            "o1".into(),
            short_enter_intent(),
            Shell::from_candle(&fire.mid()),
        );
        control
            .place_entry(1.0, 5, &entry_req(RiskBudget::Percent(1.0)))
            .await
            .expect("the order is placed");
        control.advance(fill.time);
        assert_eq!(
            control.list_open_positions("").await.unwrap().len(),
            1,
            "control: an uncancelled order fills on this H4 spread-hour bar",
        );

        // The real case: the lifecycle pulls the order before that bar.
        let b = ReplayBroker::new(vec![fire, fill], 0.0001);
        b.arm_placement(
            "o1".into(),
            short_enter_intent(),
            Shell::from_candle(&fire.mid()),
        );
        b.place_entry(1.0, 5, &entry_req(RiskBudget::Percent(1.0)))
            .await
            .expect("the order is placed");
        b.cancel_order("", "o1")
            .await
            .expect("the lifecycle pulls it");

        b.advance(fill.time);

        assert!(
            b.list_open_positions("").await.unwrap().is_empty(),
            "a cancelled order must NOT fill, however tradeable the bar looks",
        );
        assert!(
            b.held_realized_outcome("o1").is_none(),
            "a cancelled order books no outcome at all",
        );
    }
}
