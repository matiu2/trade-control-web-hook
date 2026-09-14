//! A pure candle-driven fill simulator for tick-bundle replay.
//!
//! The pure-evaluator replay (`trade-control replay`) diffs what the engine
//! *decided* (`PlanEval`). This module answers the next question: given a fired
//! **enter** intent and the candles that followed, *what would the broker have
//! done* — did the order fill, and did the position exit at its stop or target?
//!
//! It is the replay half of the roadmap's [broker simulator](../../roadmap/src/
//! broker-simulator.md), kept deliberately dumb (v1): resolve the intent's
//! entry/SL/TP via the pure [`Resolved::from_intent`], then walk the recorded
//! candles — a pending stop/limit fills when a candle's range crosses its
//! trigger; a market entry fills at once; after a fill, the first candle whose
//! range touches the stop or the target closes the position. Candles come from
//! the recorded bundle (answering the doc's open question), so the simulation is
//! deterministic and needs no broker.
//!
//! What it is **not**: it does not run the worker's `run_enter` dispatch (that
//! lives in the worker cdylib and returns a `worker::Response` that panics
//! off-wasm), so it doesn't reproduce sizing, the seen-id index, or gate
//! rejections — only the price-path fill/exit. Replaying the recorded
//! `dispatch_outcomes` through the real handlers needs the `Response` decouple
//! and is a separate, later task.
//!
//! ## Known optimism vs a real broker (deliberate v1 simplifications)
//!
//! These make the sim report outcomes a *little* better than reality. They are
//! intentional — modelling them needs live quotes / KV state the offline replay
//! doesn't have — but a future debugger should know the replay is optimistic on:
//!
//! - **Gap fills priced at the resting level.** A bar that gaps *through* a stop
//!   trigger / SL fills at the gapped book extreme on a real broker, not the
//!   requested level. We record the placed level (`book_crosses` is a boolean
//!   touch). Optimistic for stop entries and stop-loss exits.
//! - **Market entry fills at mid.** `ResolvedEntry::Market` books the mid close,
//!   not the spread-crossed price (buy the ask / sell the bid). Optimistic by the
//!   half-spread.
//! - **No KV-state or live-quote gates.** The worker's `run_enter` also applies
//!   cooldown, KV vetos, prep ordering, account caps, the `allow_entry` script,
//!   the seen-id replay check, the SL≥10×spread floor, spread-blackout,
//!   market-hours blackout, and news windows. The replay applies none of these
//!   (only the at-entry-level veto below), so it can report fires/fills the
//!   worker would reject.
//!
//! What *is* modelled faithfully (don't "simplify" these away): the fire-bar skip
//! (a pending order can't fill on the bar that fired it), same-bar fill-and-stop
//! (the fill bar is in the exit search, pessimistic on SL/TP ties), per-bar
//! bid/ask book selection, and bar-expiry (`expiry_bars` bounds the fill window).
//!
//! **Sub-bar zoom (PR-2).** The "pessimistic on SL/TP ties" default above is
//! *refined* by [`simulate_fill_resolved_zoom`]: a caller that supplies a
//! [`SubBars`] provider (a pre-fetched finer-granularity series) disambiguates an
//! exit bar whose range straddles BOTH levels by replaying its finer sub-candles
//! to see which was hit first. The pessimistic stop remains the floor — it's what
//! a caller with no finer data ([`NoZoom`]) still gets, and where even the finest
//! grain we hold is itself ambiguous.

use trade_control_core::broker::BidAskCandle;
use trade_control_core::intent::{
    Direction, Intent, Resolved, ResolvedEntry, Shell, SlWiden, widen_sl_to_spread_floor,
};
use trade_control_core::sweep_gate::{
    SweepReason, bar_adverse_extreme, bar_expiry_due, breach_detected, market_blackout_due_symbol,
};

/// What the simulator decided happened to one fired enter over the candle path.
///
/// The last two variants ([`SimOutcome::Unresolved`] / [`SimOutcome::Declined`])
/// are **pre-placement** rejections, produced only by the `#[cfg(test)]`
/// [`simulate_fill_windowed`] front door — the one that resolves a bracket from
/// scratch. The production path ([`simulate_fill_resolved_zoom`], driven by
/// `ReplayBroker`) is handed an already-`Resolved` bracket for an order the
/// broker actually placed, so by construction it can only return the first four.
/// `replay_broker`'s match still handles both defensively (drop the resting
/// order); that arm being unreachable in a non-test build is expected, not dead
/// code to delete.
#[derive(Debug, Clone, PartialEq)]
pub enum SimOutcome {
    /// The pending order never filled within the recorded candles.
    NeverFilled,
    /// Filled, but no SL/TP touched within the recorded candles — still open.
    FilledOpen {
        /// Open-time of the candle the entry filled on.
        fill_at: chrono::DateTime<chrono::Utc>,
        entry_price: f64,
    },
    /// Filled, then the stop-loss was hit.
    StoppedOut {
        fill_at: chrono::DateTime<chrono::Utc>,
        entry_price: f64,
        exit_at: chrono::DateTime<chrono::Utc>,
        exit_price: f64,
    },
    /// Filled, then the take-profit was hit.
    TookProfit {
        fill_at: chrono::DateTime<chrono::Utc>,
        entry_price: f64,
        exit_at: chrono::DateTime<chrono::Utc>,
        exit_price: f64,
    },
    /// The intent couldn't be resolved to concrete levels (not an enter, M/W
    /// not armed, invalid geometry, …). Carries the resolver's reason.
    ///
    /// Constructed only under `cfg(test)` — see the enum doc.
    #[cfg_attr(not(test), allow(dead_code))]
    Unresolved(String),
    /// The worker's at-entry level veto (Bug #12) would have rejected the
    /// entry: the resolved entry price is already past a baked
    /// `entry_level_veto` (pcl-exhausted / invalidation). No order placed.
    /// Carries the veto name (`too-low` / `too-high`). `simulate_fill`
    /// short-circuits here before any fill, mirroring `run_enter`'s gate.
    ///
    /// Constructed only under `cfg(test)` — see the enum doc.
    #[cfg_attr(not(test), allow(dead_code))]
    Declined { name: String },
}

/// Simulate one fired enter `intent` (with its triggering `candle` folded into a
/// `Shell`) against the forward candle path `candles` (ascending, the bundle's
/// `new_candles`/`detector_window`). `pip_size` is the plan's pip size, needed to
/// resolve pip-offset entry/SL levels.
///
/// The candles carry **real per-bar bid/ask books** ([`BidAskCandle`]), so the
/// fill reproduces the broker's actual spread (which widens at session opens and
/// around news) instead of a flat synthetic half-spread. You **buy at the ask,
/// sell at the bid**, so the book each leg touches depends on direction:
///
/// - **Short** (sell to open, buy to close): entry fills when the **bid** range
///   reaches the trigger; SL/TP close when the **ask** range reaches them.
/// - **Long** (buy to open, sell to close): mirror — entry on the **ask** range,
///   exits on the **bid** range.
///
/// The engine still *resolves* and evaluates on MID (the worker places every
/// level at mid); only the **fill test** here uses the relevant book side, which
/// is where the real spread lives. Recorded fill/exit prices are the placed
/// level (the resting order's price), not the book extreme that touched it.
///
/// When the data source only serves mid (bid == ask == mid per bar), this
/// degrades cleanly to exact-level mid fills.
///
/// The tick size the replay rounds order prices to, mirroring the worker's
/// fallback chain (`dispatch::enter`): the baked `Intent::tick_size` when
/// present, else `pip_size` (a safe coarser grid). Keeping this identical to the
/// worker is what preserves replay↔worker parity — both resolve through
/// `Resolved::from_intent`, which snaps the prices before its checks.
fn replay_tick(intent: &Intent, pip_size: f64) -> f64 {
    intent.tick_size.unwrap_or(pip_size)
}

/// Why the effective bracket couldn't be produced — the entry never became a
/// live position, so both the fill sim and the break-even report treat it as "no
/// fill". Each variant carries what the caller needs to render its own outcome.
///
/// Test-only, with [`resolve_effective_bracket`] — see its doc.
#[cfg(test)]
enum BracketReject {
    /// `Resolved::from_intent` failed (bad anchors / geometry).
    Unresolved(String),
    /// The entry was already past a baked at-entry level veto (Bug #12).
    LevelVeto(String),
    /// The SL-vs-spread floor widened the stop below `min_r` → declined.
    FloorRejected,
}

/// Resolve the intent to its **effective** trade bracket — the one the live
/// position actually rested on — by running the same entry-decision pipeline the
/// worker's `run_enter` does, in the same order:
///
/// 1. resolve the raw bracket (`Resolved::from_intent`, mid + tick-snapped),
/// 2. reject an entry already past a baked at-entry level veto (Bug #12),
/// 3. reject a System-1 spread-blackout entry,
/// 4. apply the SL-vs-spread floor, which **widens `stop_loss`** to the 10×
///    floor (or rejects when the wider stop drops R below `min_r`).
///
/// **Test-path only, since 2026-07-27.** Production no longer routes through
/// here: `ReplayBroker::resolved_for_sim` resolves from the *stored placed
/// levels* (the "orders are state" model — the broker already floored the stop
/// at placement, so re-deriving it would be wrong), falling back to
/// [`apply_entry_spread_floor`] only when there is no captured request.
///
/// It survives because the wrappers below ([`simulate_fill`],
/// [`breakeven_armed_at`]) are the ergonomic entry points ~40 unit tests use to
/// exercise the pre-placement rules — the entry-spread floor, the Bug #12
/// at-entry level vetos, the SL-vs-spread widen. That is real coverage of real
/// production logic, reached from a test-only front door.
///
/// It used to be the single source of truth for the floored stop, shared by the
/// fill/exit sim and [`breakeven_armed_at`] so the two could not walk the candle
/// path against *different* stop levels (the divergence that silently dropped
/// the break-even line when a wick sat between the signed and the floored SL —
/// USD/SGD iH&S replay, 2026-07-10). That invariant now lives in
/// `resolved_for_sim`, which hands the SAME `Resolved` to both.
#[cfg(test)]
fn resolve_effective_bracket(
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
    entry_spread_price: Option<f64>,
) -> Result<Resolved, BracketReject> {
    // `mut` so the SL-spread-floor widen can move `stop_loss` in place — exactly
    // as the worker's `run_enter` does — before any fill / exit / break-even
    // simulation reads it.
    let mut resolved =
        Resolved::from_intent(intent, shell, pip_size, replay_tick(intent, pip_size))
            .map_err(|err| BracketReject::Unresolved(err.to_string()))?;

    // At-entry level veto (Bug #12) — the worker's `run_enter` rejects an entry
    // already past a baked pcl-exhausted / invalidation level before any
    // placement, independent of the cross-event guard.
    let entry_ref_price = resolved.entry.reference_price();
    if let Some(elv) = intent
        .entry_level_vetos
        .iter()
        .find(|elv| elv.is_past(entry_ref_price))
    {
        return Err(BracketReject::LevelVeto(elv.name.clone()));
    }

    // (The System-1 spread-blackout rejection is NO LONGER re-derived here. The
    // replay driver now seeds the store's spread-blackout window marker on each
    // NY-close-edge bar, so `run_enter`'s OWN gate rejects a trough-spread entry
    // pre-placement — before any order is recorded — exactly as live. A rejected
    // enter never reaches this bracket resolver, so the old off-book proxy
    // (`spread_blackout_reject`) was dead and was removed. See the replay driver's
    // `set_spread_blackout_window` seed.)

    // SL-vs-spread floor SALVAGE (mirror of `run_enter`'s widen-then-reject): a
    // stop closer than `10 × spread` to entry is widened to `10 × spread` and R
    // re-checked; the widen mutates `resolved.stop_loss` so every downstream
    // reader (fill, exit, break-even) sees the widened level.
    if let EntryFloor::Rejected =
        apply_entry_spread_floor(&mut resolved, pip_size, candles, entry_spread_price)
    {
        return Err(BracketReject::FloorRejected);
    }

    Ok(resolved)
}

/// Resolve an intent's bracket from scratch and walk it — the unit-test front
/// door to [`resolve_effective_bracket`] + [`simulate_fill_resolved`].
///
/// **Test-only** (`#[cfg(test)]`), and marked so the compiler enforces it. The
/// production path is `ReplayBroker` → [`simulate_fill_resolved_zoom`], which
/// walks the *stored placed levels* instead of re-resolving. The `#[cfg]` is
/// deliberate: it was the entry point for the fixture's second fill path
/// (deleted 2026-07-27), and without the attribute nothing stops that path being
/// quietly rebuilt against it. See `fixture.rs`'s "one fill path".
#[cfg(test)]
pub fn simulate_fill(
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
) -> SimOutcome {
    // Single-sample entry floor (fire bar's own spread) — the pre-window
    // behaviour. Callers with a trailing spread window use `simulate_fill_windowed`.
    simulate_fill_windowed(intent, shell, pip_size, candles, None)
}

/// [`simulate_fill`] with an explicit `entry_spread_price` — the MEAN spread
/// over the trailing window, computed by the caller through the shared
/// `get_bidask_candles` provider + `trailing_spread_mean` (so the simulated
/// exit is floored off the SAME statistic the live worker's gate placed the
/// stop with). `None` falls back to the fire bar's own close spread — identical
/// to [`simulate_fill`].
///
/// Test-only for the same reason as [`simulate_fill`].
#[cfg(test)]
pub fn simulate_fill_windowed(
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
    entry_spread_price: Option<f64>,
) -> SimOutcome {
    // Resolve the effective (floored) bracket through the shared pipeline — the
    // single place the entry-decision gates + SL-spread floor are applied, so the
    // fill/exit sim and `breakeven_armed_at` can't walk against different stops.
    // Each rejection maps to the outcome the worker's `run_enter` would have
    // produced.
    let resolved =
        match resolve_effective_bracket(intent, shell, pip_size, candles, entry_spread_price) {
            Ok(r) => r,
            Err(BracketReject::Unresolved(err)) => return SimOutcome::Unresolved(err),
            Err(BracketReject::LevelVeto(name)) => return SimOutcome::Declined { name },
            Err(BracketReject::FloorRejected) => {
                return SimOutcome::Declined {
                    name: "sl-widen-below-min-r".to_string(),
                };
            }
        };
    simulate_fill_resolved(&resolved, intent, shell, pip_size, candles)
}

/// A source of **finer-granularity** bid/ask candles the exit sim can zoom into
/// when a single bar's range straddles BOTH the stop-loss and the take-profit —
/// the one case the coarse bar can't order (did price hit SL or TP first?).
///
/// The offline replay pre-fetches a finer series (e.g. M1 under an H1 plan) once
/// and implements this; the pure sim stays sync and simply *consults* the
/// pre-fetched data — no async fetch is threaded through the engine. When no
/// finer data is available for the ambiguous bar (`sub_bars` returns an empty
/// slice), the sim keeps the pessimistic-stop assumption, so a caller with no
/// provider ([`NoZoom`]) is byte-identical to the pre-zoom behaviour.
pub trait SubBars {
    /// Finer bid/ask candles whose open-time falls in the half-open window
    /// `[start, end)` — the sub-bars of one coarse parent bar `[start, end)`,
    /// ascending. Empty ⇒ no finer data for this window (fall back to pessimism).
    fn sub_bars(
        &self,
        start: chrono::DateTime<chrono::Utc>,
        end: chrono::DateTime<chrono::Utc>,
    ) -> Vec<BidAskCandle>;
}

/// The no-op [`SubBars`] provider: never returns finer candles, so the exit sim
/// keeps the pessimistic-stop assumption on an ambiguous bar. This is what every
/// caller that doesn't pre-fetch a finer series uses (all `engine` unit tests,
/// the fixture re-sim, the pre-zoom `simulate_fill*` entry points), which is why
/// their outcomes are unchanged by the zoom machinery.
pub struct NoZoom;

impl SubBars for NoZoom {
    fn sub_bars(
        &self,
        _start: chrono::DateTime<chrono::Utc>,
        _end: chrono::DateTime<chrono::Utc>,
    ) -> Vec<BidAskCandle> {
        Vec::new()
    }
}

/// The pure fill/exit physics over a bracket the caller has ALREADY resolved +
/// floored — the "orders are state" entry point. This is the body
/// [`simulate_fill_windowed`] runs after `resolve_effective_bracket`; extracting
/// it lets the `ReplayBroker` walk its **stored placed levels** (the stop the
/// broker actually rests on) without re-resolving the intent or re-deriving the
/// SL-vs-spread floor off a trailing spread — which is what made the retry-gate
/// `resolve` and the ledger `realize` disagree (replay↔live divergence #4). One
/// bracket in, one outcome out; no spread scalar, no floor.
///
/// `intent` + `shell` are still needed for the entry-side fill test (`find_fill`
/// keys the pending-order trigger + the spread-hour rubbish-candle skip off them)
/// and for the System-2 widen's per-instrument spread-hour gate — but the SL/TP
/// **levels** come from `resolved`, never re-floored.
///
/// On an ambiguous exit bar (range straddles both SL and TP) this keeps the
/// pessimistic-stop assumption. A caller with a finer series should use
/// [`simulate_fill_resolved_zoom`] to disambiguate instead.
///
/// Test-only: production always has a zoom source to offer, so it calls
/// [`simulate_fill_resolved_zoom`] directly. This is the `NoZoom` convenience.
#[cfg(test)]
pub fn simulate_fill_resolved(
    resolved: &Resolved,
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
) -> SimOutcome {
    simulate_fill_resolved_zoom(resolved, intent, shell, pip_size, candles, &NoZoom)
}

/// [`simulate_fill_resolved`] with a [`SubBars`] provider: on an exit bar whose
/// range straddles BOTH SL and TP, it replays that bar's finer sub-candles (in
/// order) to decide which level was touched first, instead of pessimistically
/// assuming the stop. A sub-bar that is *itself* still ambiguous, or no finer
/// data at all, degrades to the pessimistic stop — the finest grain we hold is
/// the floor, zoom only ever REDUCES the ambiguity.
///
/// Break-even and the System-2 widen are computed at the **parent-bar** grain
/// (both latch off a bar's CLOSE, so they can only change the effective stop on
/// the NEXT parent bar, never mid-bar); the sub-bars are tested against the
/// parent bar's already-resolved `effective_stop` / `take_profit`.
pub fn simulate_fill_resolved_zoom(
    resolved: &Resolved,
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
    sub_bars: &dyn SubBars,
) -> SimOutcome {
    let dir = resolved.direction;

    // Phase 1 — find the fill (shared with `breakeven_armed_at`).
    let Some(fill) = find_fill(resolved, intent, shell, dir, candles) else {
        return SimOutcome::NeverFilled;
    };
    let (fill_at, entry_price, rest) = (fill.fill_at, fill.entry_price, fill.rest);

    // Phase 2 — after the fill, the first candle that touches SL or TP closes
    // the position. The close is the *opposite* book side from entry (short buys
    // back on the ask, long sells on the bid). If both are touched in the same
    // candle we can't tell the intrabar order from a closed bar, so we
    // conservatively call it the stop (the worse outcome) — matches the
    // simulator doc's "exact-level, pessimistic on ambiguity" stance.
    //
    // Break-even management (BUG-replay-no-breakeven-stop-at-50pct): if the
    // enter carries `breakeven`, the active stop starts at the resolved SL and
    // moves to the entry price once a candle *closes* past the 50%-to-TP level.
    // Latched / one-way. The operator's same-bar rule: BE arms on a close, so
    // the moved stop is live from the **next** bar — on the arming bar the
    // original (or already-moved) stop still applies, mirroring the broker's
    // resting stop. We therefore test the exit against `active_stop` first, then
    // arm BE from this candle's close for subsequent bars.
    let exit_book = book_for(Leg::Exit, dir);
    let be_arms_at = resolved
        .breakeven
        .map(|be| be.arms_at(entry_price, resolved.take_profit));

    // System-2 spread-hour widen (replay==live): during a learned spread hour the
    // live cron amends the broker stop *away* from price, transiently, then the
    // recovery watcher restores it. The SHARED reconstruction computes that same
    // widen from the SAME placed bracket + candle path, measured relative to
    // `resolved.stop_loss` (the stored placed stop), so the exit is scored against
    // the stop the LIVE broker would actually hold — not the un-widened level.
    // The widen governs bars in `[effective_from, restored_at)`; outside it the
    // break-even-managed `active_stop` applies.
    let widen_trigger =
        trade_control_core::spread_blackout::elevated_threshold_pips(&intent.instrument);
    // EVERY widen episode, not just the first: a position open for days crosses
    // several spread hours and the live cron widens at each. Scoring the exit off
    // one episode left later spread hours bare and booked stop-outs the live
    // worker would have carried (AUD/NZD 2026-06-11: −1.00R vs +1.18R).
    let widen_episodes = trade_control_core::order_control::WidenEpisodes::new(
        widen_episodes_at_resolved(resolved, intent, shell, pip_size, candles, widen_trigger)
            .into_iter()
            .map(|w| trade_control_core::order_control::WidenEpisode {
                effective_from: w.effective_from,
                restored_at: w.restored_at,
                widened_stop: w.widened_stop,
            })
            .collect(),
    );

    // The parent-bar length, to bound each bar's sub-window `[c.time, c.time +
    // bar_len)` when we zoom. Inferred from the exit-window spacing (the smallest
    // positive gap between consecutive bars) so a session gap between two bars
    // doesn't inflate it; zero when `rest` has < 2 bars (⇒ no zoom, pessimistic).
    let bar_len = infer_bar_len(rest);
    // The bar cadence in minutes, for the shared break-even noise floor's ATR
    // length. Derived from the same inferred `bar_len` the zoom uses rather than
    // from a `Granularity` the fill sim does not carry; zero (a single-bar
    // series) simply leaves the ATR unwarmed, which the floor treats as
    // unjudgeable and fails open on.
    let bar_minutes = bar_len.num_minutes();
    let mut active_stop = resolved.stop_loss;
    for (i, c) in rest.iter().enumerate() {
        // The stop the live broker holds on THIS bar: the widened stop while the
        // transient widen is active (`[effective_from, restored_at)`), else the
        // break-even-managed stop. The widen moves the stop AWAY from price, so it
        // can only ever protect (loosen) — it never fabricates a tighter exit.
        let effective_stop = widen_episodes.stop_on_bar(c.time, active_stop);
        let hit_sl = book_reaches(c, exit_book, effective_stop, stop_approach(dir));
        let hit_tp = book_reaches(c, exit_book, resolved.take_profit, tp_approach(dir));
        match (hit_sl, hit_tp) {
            // Only the STOP touched — unambiguous, exit at the stop.
            (true, false) => {
                return SimOutcome::StoppedOut {
                    fill_at,
                    entry_price,
                    exit_at: c.time,
                    exit_price: effective_stop,
                };
            }
            // Only the TARGET touched — unambiguous, exit at the target.
            (false, true) => {
                return SimOutcome::TookProfit {
                    fill_at,
                    entry_price,
                    exit_at: c.time,
                    exit_price: resolved.take_profit,
                };
            }
            // BOTH touched in one bar — the coarse bar can't order them. Zoom into
            // this bar's finer sub-candles to see which level was hit first;
            // pessimistic stop only if the finer data can't resolve it either.
            (true, true) => {
                return zoom_ambiguous_bar(
                    c,
                    bar_len,
                    sub_bars,
                    exit_book,
                    effective_stop,
                    resolved.take_profit,
                    stop_approach(dir),
                    tp_approach(dir),
                    fill_at,
                    entry_price,
                );
            }
            (false, false) => {}
        }
        // Arm break-even for the next bar onward when this candle CLOSES past
        // the 50% level. Latched: once moved to entry it never reverts (a long's
        // entry >= original SL, a short's entry <= it, so `active_stop` only
        // tightens).
        //
        // TWO independent gates gate this one arming site, and they compose:
        // Rule 2 decides whether this BAR may arm at all; the noise floor then
        // decides whether the STOP it would produce is sane. Neither subsumes
        // the other — a bar outside every widen can still yield an absurd
        // target, and a bar inside one is refused however sane its target looks.
        //
        // Rule 2: a bar INSIDE an active widen does not arm. Those are the same
        // "rubbish candles" the engine suppresses entries, detection and crosses
        // on, so a 50%-to-TP close printed by one is much more likely to be the
        // spread than the market. The crossing is forgotten, not deferred — a
        // fresh reading is taken from the first ordinary bar after the restore
        // (the restore bar itself counts as ordinary, since the live cron has
        // already amended the stop back by then). See
        // `core::order_control::in_force_stop` for the full reasoning, including
        // the honest note that there is no statistical evidence for this choice.
        //
        // THE NOISE FLOOR (finding #8 of the 2026-09-13 replay↔live divergence
        // audit). The live cron refuses an amend whose target lands within
        // `BREAKEVEN_MIN_ATR_FRACTION × ATR` of the latest close and keeps the
        // ORIGINAL stop (`breakeven_decision::decide` →
        // `BreakevenBlock::InsideNoise`); replay had no such check, so a
        // mis-derived target was loud on live and silent offline — where it books
        // a ~0R scratch on the next bar's noise. The predicate is the SHARED one
        // in `core::order_control::breakeven_noise`, called from both halves
        // (`[[strategy_changes_in_both_replayer_and_worker]]`); do not
        // re-implement the comparison here.
        //
        // It is a tripwire for absurdity, not a tuning knob: a correctly-derived
        // break-even sits ~50% of the way to TP, many multiples of ATR clear of
        // this line, so it should never fire on a real setup. Consequently NO
        // FIXTURE IS EVIDENCE about it — the whole corpus is expected to be
        // unchanged — and the tests that prove it works construct deliberately
        // absurd targets instead.
        if let (Some(be), Some(level)) = (resolved.breakeven, be_arms_at)
            && trade_control_core::order_control::breakeven_arm_gate(&widen_episodes, c.time)
                .armable()
            && be.close_arms(dir, level, c.c)
        {
            let new_stop = be.target_stop(entry_price);
            // The window the floor judges against: the post-fill bars up to and
            // INCLUDING this one, mid prices. Structurally the same window the
            // live cron's `armable_candles` yields at the tick that would make
            // this amend — bars that closed at or after the fill, latest last —
            // so both halves measure the same ATR against the same reference
            // close.
            let window: Vec<_> = rest[..=i].iter().map(BidAskCandle::mid).collect();
            let verdict = trade_control_core::order_control::judge_breakeven_stop_at_bar_minutes(
                &window,
                new_stop,
                bar_minutes,
            );
            if verdict.blocks() {
                // Loud offline, exactly as it is loud live: the operator must be
                // able to SEE a refused break-even in a replay run rather than
                // inferring it from an unexpectedly full stop-out.
                tracing::warn!(
                    "replay: REFUSING break-even to {new_stop} at {} — inside the noise floor \
                     ({verdict:?}). The original stop stands, matching the live cron's \
                     BreakevenBlock::InsideNoise. A break-even this close to market is not a \
                     scratch; something upstream produced a wrong target.",
                    c.time,
                );
            } else {
                active_stop = new_stop;
            }
        }
    }

    SimOutcome::FilledOpen {
        fill_at,
        entry_price,
    }
}

/// The modal bar length of an ascending candle slice — the smallest strictly
/// positive gap between consecutive open-times. Used to bound a bar's sub-window
/// `[c.time, c.time + bar_len)` for the zoom. A session gap (weekend / close)
/// yields a *larger* gap on one pair, so taking the **min** positive gap gives
/// the true bar cadence rather than the gap width. `Duration::zero()` when the
/// slice has fewer than two bars (⇒ the zoom window is empty ⇒ no sub-bars ⇒
/// pessimistic stop, the safe fallback).
fn infer_bar_len(candles: &[BidAskCandle]) -> chrono::Duration {
    candles
        .windows(2)
        .map(|w| w[1].time - w[0].time)
        .filter(|d| *d > chrono::Duration::zero())
        .min()
        .unwrap_or_else(chrono::Duration::zero)
}

/// Resolve an exit bar whose range straddles BOTH the stop and the target by
/// replaying its finer sub-candles in order. The first sub-bar that touches the
/// stop → [`SimOutcome::StoppedOut`]; the first that touches the target →
/// [`SimOutcome::TookProfit`]. A sub-bar that itself straddles both (still
/// ambiguous at the finest grain we hold) → the pessimistic stop, as does an
/// empty sub-window ([`NoZoom`] / no finer data / a zero `bar_len`). The exit is
/// stamped at the PARENT bar's open-time (`c.time`) either way — the sub-bars
/// only decide *which* level, not a finer timestamp, keeping the exit bar aligned
/// with the coarse series the rest of the report walks.
#[allow(clippy::too_many_arguments)]
fn zoom_ambiguous_bar(
    c: &BidAskCandle,
    bar_len: chrono::Duration,
    sub_bars: &dyn SubBars,
    exit_book: Book,
    effective_stop: f64,
    take_profit: f64,
    stop_approach: Approach,
    tp_approach: Approach,
    fill_at: chrono::DateTime<chrono::Utc>,
    entry_price: f64,
) -> SimOutcome {
    let stopped = SimOutcome::StoppedOut {
        fill_at,
        entry_price,
        exit_at: c.time,
        exit_price: effective_stop,
    };
    let took_profit = SimOutcome::TookProfit {
        fill_at,
        entry_price,
        exit_at: c.time,
        exit_price: take_profit,
    };
    // Empty/zero window ⇒ no finer data ⇒ keep the pessimistic stop.
    if bar_len <= chrono::Duration::zero() {
        return stopped;
    }
    for sub in sub_bars.sub_bars(c.time, c.time + bar_len) {
        let sub_sl = book_reaches(&sub, exit_book, effective_stop, stop_approach);
        let sub_tp = book_reaches(&sub, exit_book, take_profit, tp_approach);
        match (sub_sl, sub_tp) {
            (true, false) => return stopped,
            (false, true) => return took_profit,
            // A sub-bar straddling both is ambiguous at the finest grain we have:
            // stay pessimistic (this sub-bar is the first to touch EITHER level, so
            // no later sub-bar can pre-empt it — return now).
            (true, true) => return stopped,
            (false, false) => {}
        }
    }
    // No sub-bar touched either level (finer data didn't cover the move) — keep
    // the pessimistic stop rather than silently reporting the position still open.
    stopped
}

// The old `spread_blackout_reject` proxy was removed: the replay driver now seeds
// the store's spread-blackout window marker per NY-close-edge bar, so
// `run_enter`'s OWN System-1 gate rejects a trough-spread entry pre-placement
// (via `ReplayBroker::get_quote`) — the offline decision is the live gate itself,
// not an off-book re-derivation. `core::spread_blackout::{spread_blackout_decision,
// elevated_threshold_pips}` remain the shared decision, now called only from
// `run_enter`.

/// A located fill: when/where the entry order filled, and the candle slice from
/// the fill bar onward (the post-fill SL/TP/break-even search window). Shared by
/// [`simulate_fill`] and [`breakeven_armed_at`] so the two can't disagree on
/// *which* bar the order filled on.
struct Fill<'a> {
    fill_at: chrono::DateTime<chrono::Utc>,
    entry_price: f64,
    rest: &'a [BidAskCandle],
}

/// Phase 1 of the fill: find where the entry order filled. A short entry sells
/// (fills on the bid book); a long entry buys (fills on the ask book). A market
/// entry crosses the spread at once. The recorded fill price is the placed level
/// (the resting order's price), not the book extreme.
///
/// `candles[0]` is the **fire bar** — the bar the enter fired on. A pending
/// order is only placed once that bar has *closed* (the engine decides the fire
/// on the cron tick that processes the closed bar; under `needs_confirmed` the
/// confirmation itself isn't known until this bar's close). So a Stop/Limit order
/// cannot interact with the fire bar's own intrabar path — the earliest bar it
/// can fill on is the **next** one, so we search from `candles[1..]`. A Market
/// entry is the exception: it fills at the fire bar's close (the shell price).
///
/// The bar length in seconds inferred from the smallest positive gap between
/// consecutive candle open-times in `candles`. Weekend/session gaps are larger
/// multiples of the true bar length, so the **minimum** positive delta is the
/// bar size (H1 = 3600, H4 = 14400, …). Returns `0` for a window with fewer
/// than two distinct times — callers treat that as "unknown, fail safe".
fn bar_seconds_of(candles: &[BidAskCandle]) -> i64 {
    candles
        .windows(2)
        .map(|w| w[1].time.timestamp() - w[0].time.timestamp())
        .filter(|d| *d > 0)
        .min()
        .unwrap_or(0)
}

/// Bound a resting order's fill window at the bar the **live cron sweep** would
/// have cancelled it on for a pre-fill SL breach.
///
/// The worker's `sweep_pending_orders` walks every still-resting `EntryAttempt`
/// each tick and cancel-and-deletes any whose stop-loss current price has already
/// overtaken (`trade-control-cron/src/sweep.rs`, the `maybe_breach_cancel` arm):
/// the setup invalidated *before* it ever filled, so the order is dead and can
/// never fill afterwards. Offline there is no sweep driver, so a resting order
/// whose SL was blown through sat on the books and filled if price later came
/// back through its trigger — inventing a trade production structurally could not
/// take, and booking its R into the golden corpus.
///
/// This mirrors the sweep's effect on the fill path exactly as `expiry_bars`
/// already mirrors the `bar-expiry` arm above: truncate the window at the first
/// bar that trips the sweep, so a cross on a later bar is an order the worker
/// would already have cancelled.
///
/// The breach predicate is the **shared** [`breach_detected`] from
/// `core::sweep_gate` — the same one the live sweep and the replay's reporting
/// [`sweep_reason`] call — so worker and replay cannot drift
/// (`[[strategy_changes_in_both_replayer_and_worker]]`).
///
/// # WICK, not close (2026-09-14) — and why close was rejected
///
/// The rule both sides implement is **"has price TRADED past the stop since
/// placement"**, so this reads each bar's **adverse extreme** — the low for a
/// Long, the high for a Short — via `core::sweep_gate::bar_adverse_extreme`.
///
/// It used to read the bar's mid **close**, on the theory that the live cron
/// samples one point price per tick rather than a bar range. That was a faithful
/// mirror of what live *did*, and both were wrong together: live read an
/// instantaneous spot quote, so its answer depended on where the cron tick
/// landed on the price path, and an excursion between two ticks was invisible.
/// Live is now stateful (a running adverse extreme persisted on the
/// `EntryAttempt`, see `trade-control-cron/src/sweep.rs::maybe_breach_cancel`),
/// which makes it monotonic; the bar's adverse extreme is the bar-resolution
/// analogue of the same question. Close-sampling was explicitly **REJECTED**
/// because it lets a bar trade clean through the stop and back inside with the
/// order surviving — the exact case the rule exists to catch.
///
/// The mid book is deliberate on both sides: the live quote is a mid quote, and
/// the question is where the *market* went, not which book the order would have
/// filled on.
///
/// # The fixture corpus CANNOT justify this rule
///
/// Measured over the full 2847-cell corpus
/// (`EXPERIMENT-pre-fill-sl-breach-sweep.md`): close-mode truncation fired on
/// **854 orders** and changed the outcome of **ZERO** of them — disabling the
/// truncation entirely was byte-identical. That is because the rule only changes
/// an outcome when a breached order would *later* return through its trigger and
/// fill, which inside an alert window essentially never happens. It is a no-op
/// on *outcomes*, not a no-op in *mechanism*. And the goldens were themselves
/// recorded under close-sampling, so by construction they contain almost no bar
/// that wicked past the stop and closed back inside. **A green corpus after
/// deleting this proves nothing — do not simplify it away on that strength.**
///
/// Scoped to the pre-fill window only. A breach *after* the fill is the
/// position's own stop-out, which Phase 2 already handles — the sweep only ever
/// touches a resting order, never a filled position.
///
/// The breaching bar itself is **kept**: the sweep runs on the cron tick that
/// observes that bar's close, and price could have reached the trigger earlier in
/// that same bar, so the order was still live during it. Only the bars *after*
/// the breach are cut. This matches the `expiry_bars` precedent, whose bound is
/// likewise exclusive of the bar the worker cancels on.
fn truncate_at_pre_fill_sl_breach<'a>(
    fill_window: &'a [BidAskCandle],
    resolved: &Resolved,
    dir: Direction,
) -> &'a [BidAskCandle] {
    match fill_window
        .iter()
        .position(|c| breach_detected(dir, bar_adverse_extreme(dir, c.h, c.l), resolved.stop_loss))
    {
        // `+ 1` keeps the breaching bar in the window: the order was still live
        // *during* it, and only the sweep tick at its close kills it.
        Some(breach) => fill_window.get(..breach + 1).unwrap_or(fill_window),
        None => fill_window,
    }
}

/// Returns `None` when the pending order never fills within the window (the
/// caller maps that to `NeverFilled`).
fn find_fill<'a>(
    resolved: &Resolved,
    intent: &Intent,
    shell: &Shell,
    dir: Direction,
    candles: &'a [BidAskCandle],
) -> Option<Fill<'a>> {
    let entry_book = book_for(Leg::Entry, dir);
    match resolved.entry {
        ResolvedEntry::Market { reference_price } => Some(Fill {
            fill_at: shell.time,
            entry_price: reference_price,
            rest: candles,
        }),
        ResolvedEntry::Stop { trigger_price } | ResolvedEntry::Limit { trigger_price } => {
            // The side price must approach the trigger from for the order to
            // fill. A *stop* sits on the far side of the market in the trade's
            // direction (long-stop above → price rises into it → `FromBelow`;
            // short-stop below → price falls into it → `FromAbove`). A *limit*
            // is the mirror (long-limit below → `FromAbove`). Using the
            // directional `book_reaches` (not the old bracket test) is what lets
            // a bar that *gaps through* the trigger fill — the bug this fixes.
            let entry_approach = match (&resolved.entry, dir) {
                (ResolvedEntry::Stop { .. }, Direction::Long)
                | (ResolvedEntry::Limit { .. }, Direction::Short) => Approach::FromBelow,
                (ResolvedEntry::Stop { .. }, Direction::Short)
                | (ResolvedEntry::Limit { .. }, Direction::Long) => Approach::FromAbove,
                // Market handled above; unreachable in this arm.
                (ResolvedEntry::Market { .. }, _) => Approach::FromBelow,
            };
            // Skip the fire bar (index 0): the resting order isn't live until
            // after it closes. `get(1..)` is empty when the fire bar is the only
            // recorded candle, yielding `None` (no later bar to fill on yet).
            let after_fire = candles.get(1..).unwrap_or(&[]);
            // Bar-expiry (`expiry_bars`): the worker cancels a still-resting order
            // `N` bars after the fire bar (its `cancel_at = next_candle_timestamp_N`).
            // Mirror that here by bounding the fill window to the first `N` bars
            // after the fire bar — a cross on a later bar is an order the worker
            // would already have cancelled, so it must not fill. A static
            // `expiry_bars` is honoured; a script-resolved one (Rhai) is beyond
            // this pure price-path sim, so it's treated as "no bar-expiry"
            // (unbounded), same as `None`.
            let expiry_bars = intent
                .expiry_bars
                .as_ref()
                .and_then(|t| t.as_static())
                .copied();
            let fill_window: &[BidAskCandle] = match expiry_bars {
                Some(n) => after_fire
                    .get(..(n as usize).min(after_fire.len()))
                    .unwrap_or(&[]),
                None => after_fire,
            };
            // Spread-hour "rubbish candle": a pending Stop/Limit whose trigger is
            // first reached on a learned spread hour must NOT fill there — the
            // bar is a liquidity-vacuum spike, not a real touch (the AUD/CHF
            // 2026-07-08 fill-into-the-spread-hour case). The order stays resting
            // and fills on the next clean bar. Mirrors the worker's entry gate so
            // replay == live. See SCOPING-spread-hour-rubbish-candle.md.
            //
            // PR 4b-3 relationship (KEPT — option a, not removed): the shared
            // `pending_order_lifecycle` now *cancels* a resting order in a spread
            // hour at the broker, and `resolve()` reads THIS skip to report the
            // order as still-`Pending` on the spike bar — which is exactly what
            // lets the lifecycle *see* it as resting and cancel it. So the two are
            // complementary, not duplicate: this skip keeps the fill from landing
            // on the spike (and is the resting-state the lifecycle acts on); the
            // lifecycle is the live-matching cancel+backup. Removing this skip
            // would (1) break the engine's own `pending_stop_does_not_fill_*`
            // tests and (2) make `resolve()` report the order FILLED on the spike
            // bar, so the lifecycle would never see it resting to cancel it.
            // Bar length inferred from consecutive candle times, so the spread-
            // hour fill skip is gated on bar size exactly as the engine's entry
            // suppression is (only 15m/1h are dominated by a 1h spread hour; H4+
            // fill normally). Derived here rather than threaded as `Granularity`
            // to avoid churning every simulator signature. `0` (a one-candle
            // window) fails safe to "short bar" in the helper → still skips.
            let bar_seconds = bar_seconds_of(candles);
            let fill_window = truncate_at_pre_fill_sl_breach(fill_window, resolved, dir);
            let i = fill_window.iter().position(|c| {
                book_reaches(c, entry_book, trigger_price, entry_approach)
                    && !trade_control_core::spread_blackout::suppress_on_spread_hour_bar_seconds(
                        &intent.instrument,
                        c.time,
                        bar_seconds,
                    )
            })?;
            // `i` indexes `fill_window` (a prefix of `after_fire`), so the fill
            // bar is `candles[i + 1]`. The post-fill search **includes** the fill
            // bar itself (`candles[i + 1..]`): an order that fills mid-bar can be
            // stopped out (or hit TP) later in that SAME bar.
            Some(Fill {
                fill_at: after_fire[i].time,
                entry_price: trigger_price,
                rest: &candles[i + 1..],
            })
        }
    }
}

/// The bar on which break-even **would arm** for this enter — i.e. the first
/// post-fill candle whose *close* runs past the `breakeven` threshold (50%-to-TP
/// by default), at or before the position's exit. `None` when the enter carries
/// no `breakeven`, never fills, or exits (SL/TP) before any candle arms it.
///
/// This is the **replay stand-in for the live cron amend**: in production
/// [`crate::breakeven_watch`] doesn't move the stop at fill-time — it runs every
/// 15-min cron tick and sends `amend_stop(entry)` to the broker on the first tick
/// that observes a closed candle past the threshold. The bar returned here is
/// exactly that candle, so a replay can show "this is when the worker would have
/// amended the broker SL to break-even." It shares the same arming predicate
/// ([`Breakeven::close_arms`]), fill-finding ([`find_fill`]), and SL-spread floor
/// ([`apply_entry_spread_floor`]) as the fill simulator, so the reported bar can't
/// drift from the simulated outcome. `entry_spread_price` is the same trailing
/// mean the report feeds `simulate_fill_windowed`.
///
/// Pure and side-effect-free. The bare (intent-resolving) form is now exercised
/// only by this module's tests — production uses the `_resolved` variant — so it
/// is `#[cfg(test)]`.
#[cfg(test)]
fn breakeven_armed_at(
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
    entry_spread_price: Option<f64>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    // Resolve the effective (floored) bracket through the SAME shared pipeline the
    // fill sim uses. Any rejection (unresolved / level-veto / spread-blackout /
    // floor-below-min-r) means the worker never placed the entry, so break-even
    // never armed → `None`. Crucially, walking against the shared floored stop is
    // what stops a wick between the signed and floored SL from falsely reporting
    // "stopped out before arming" and suppressing the SL→break-even line.
    let resolved =
        resolve_effective_bracket(intent, shell, pip_size, candles, entry_spread_price).ok()?;
    breakeven_armed_at_resolved(&resolved, intent, shell, pip_size, candles)
}

/// [`breakeven_armed_at`] over a bracket the caller has ALREADY resolved + floored
/// — the "orders are state" display path, where the report reads the broker's
/// stored placed bracket so its SL→break-even line arms off the SAME floored stop
/// the ledger scored (no trailing-spread re-derivation).
pub fn breakeven_armed_at_resolved(
    resolved: &Resolved,
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
) -> Option<chrono::DateTime<chrono::Utc>> {
    let be = intent.breakeven?;
    let dir = resolved.direction;
    let fill = find_fill(resolved, intent, shell, dir, candles)?;

    let exit_book = book_for(Leg::Exit, dir);
    let level = be.arms_at(fill.entry_price, resolved.take_profit);
    // Rule 2 must reach this walk too, or the JOURNAL and the SCORED OUTCOME
    // disagree: the report would print "SL→break-even" against a bar
    // `simulate_fill` refused to arm from, and the operator reads a trace that
    // contradicts its own R. This is a second walk of the same path (the "orders
    // are state" display path), so it needs the same gate — reconstructed from
    // the same shared episode function, not re-derived here.
    let widen_trigger =
        trade_control_core::spread_blackout::elevated_threshold_pips(&intent.instrument);
    let widen_episodes = trade_control_core::order_control::WidenEpisodes::new(
        widen_episodes_at_resolved(resolved, intent, shell, pip_size, candles, widen_trigger)
            .into_iter()
            .map(|w| trade_control_core::order_control::WidenEpisode {
                effective_from: w.effective_from,
                restored_at: w.restored_at,
                widened_stop: w.widened_stop,
            })
            .collect(),
    );
    // Walk the post-fill path exactly as Phase 2 does: an exit (SL/TP) before any
    // arming close means break-even never armed during the position's life.
    for c in fill.rest {
        if book_reaches(c, exit_book, resolved.stop_loss, stop_approach(dir))
            || book_reaches(c, exit_book, resolved.take_profit, tp_approach(dir))
        {
            return None;
        }
        let gate = trade_control_core::order_control::breakeven_arm_gate(&widen_episodes, c.time);
        if gate.armable() && be.close_arms(dir, level, c.c) {
            return Some(c.time);
        }
    }
    None
}

/// Why the live cron sweep would have cancelled a `NeverFilled` resting order,
/// and the bar timestamp at which it would have acted — the replay stand-in for
/// the worker's [`sweep_pending_orders`](../../../src/cron/sweep.rs).
///
/// `simulate_fill` reports `NeverFilled` for *any* order that never triggered —
/// but the live worker doesn't passively wait: every cron tick it walks each
/// resting `EntryAttempt` and **cancels** it once its alert window expired, its
/// bar-based `cancel_at` passed, it sits inside a market-hours blackout, or
/// current price overtook its stop-loss. A replay that can't tell an order the
/// worker would have *swept* from one that merely never triggered diverges
/// silently from production. This walks the post-fire candle path and returns
/// the first sweep the worker would have made.
///
/// Mirrors the worker's `sweep_one` branch priority at each bar: **expired**
/// (alert window) → **bar-expiry** (`cancel_at`) → **blackout** (market-hours)
/// → **SL-breach**. It reuses the shared `core::sweep_gate` predicates so worker
/// and replay can't drift, and derives `cancel_at` via the same
/// `core::resolve_cancel_at` the worker uses (off the Pine-shipped forward
/// bar-close menu on the shell).
///
/// The market-hours blackout is now read from the **baked, weekday-aware**
/// table keyed on `intent.instrument` (`core::sweep_gate::market_blackout_due_symbol`),
/// the same predicate the live worker's reject gate uses — so a blackout-driven
/// sweep here matches production with no `market_info` fetch and no window
/// plumbing. An instrument not in the baked catalog fails open (no fabricated
/// blackout), exactly the worker's behaviour, and the order falls through to the
/// SL-breach check / plain "never triggered" verdict.
///
/// Returns `None` when no sweep condition is reached within the candle path, or
/// when the order would never rest (a Market entry / an unresolved intent — the
/// caller's `NeverFilled` is then not a swept order).
///
/// Pure and side-effect-free; the report calls it independently of
/// [`simulate_fill`] so the `SimOutcome` enum (and every saved fixture) stays
/// untouched — the same pattern [`breakeven_armed_at`] uses.
pub fn sweep_reason(
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
) -> Option<(SweepReason, chrono::DateTime<chrono::Utc>)> {
    let resolved =
        Resolved::from_intent(intent, shell, pip_size, replay_tick(intent, pip_size)).ok()?;

    // A Market entry fills at once (it never rests), so a `NeverFilled` Market is
    // not a swept order — there's nothing for the sweep to cancel.
    let sl = match resolved.entry {
        ResolvedEntry::Stop { .. } | ResolvedEntry::Limit { .. } => resolved.stop_loss,
        ResolvedEntry::Market { .. } => return None,
    };
    let dir = resolved.direction;

    // Derive the bar-based `cancel_at` exactly as the worker's `run_enter` does:
    // off the Pine-shipped forward bar-close menu on the shell, capped at the
    // alert window. A non-static / out-of-range / absent `expiry_bars` yields no
    // bar-expiry (matching the worker, which only sets `cancel_at` when it
    // resolves cleanly).
    let cancel_at = intent
        .expiry_bars
        .as_ref()
        .and_then(|t| t.as_static())
        .copied()
        .and_then(|bars| {
            trade_control_core::intent::resolve_cancel_at(bars, shell, intent.not_after).ok()
        });

    // The order rests from the bar *after* the fire bar (a pending order isn't
    // live until the fire bar closes — same skip `find_fill` applies). Walk those
    // live bars chronologically; the first that trips a sweep branch wins.
    for c in candles.get(1..).unwrap_or(&[]) {
        if intent.not_after < c.time {
            return Some((SweepReason::Expired, c.time));
        }
        if bar_expiry_due(cancel_at, c.time) {
            return Some((SweepReason::BarExpiry, c.time));
        }
        // Market-hours blackout: the resting order is caught inside the
        // instrument's daily close→open gap. Runs BEFORE SL-breach to match the
        // worker's `sweep_one` ordering — across a closed session a price-based
        // check would read a stale quote, so the closed market itself is the
        // trigger. Reads the baked weekday-aware mask keyed on the instrument
        // (same predicate as the worker's reject gate); an uncatalogued symbol
        // fails open (no fabricated blackout).
        if market_blackout_due_symbol(&intent.instrument, c.time) {
            return Some((SweepReason::Blackout, c.time));
        }
        // SL-breach reads the bar's ADVERSE EXTREME (low for a Long, high for a
        // Short), not its close — the bar-resolution form of the rule both sides
        // implement: "has price TRADED past the stop since placement".
        //
        // This mirrors `truncate_at_pre_fill_sl_breach`, which is the arm that
        // actually alters outcomes; keeping the two in step is what stops the
        // journal from labelling an order `sl-breached` at a *later* bar than
        // the one the fill window was truncated at. See that function's doc for
        // the full why — in short: live used to read an instantaneous spot quote
        // (tick-alignment lottery) and now carries a persisted running extreme,
        // and close-sampling here was REJECTED because it lets a bar trade
        // through the stop and back with the order surviving.
        if breach_detected(dir, bar_adverse_extreme(dir, c.h, c.l), sl) {
            return Some((SweepReason::SlBreached, c.time));
        }
    }

    None
}

/// Outcome of the System-1 entry SL-spread floor applied to a resolved bracket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EntryFloor {
    /// The floor left the stop where it was (already ≥ 10× spread), or moved it
    /// to `10 × spread`. `spread_pips` is the fire-bar spread the floor used —
    /// surfaced so the journal can show *which* spread sized the placed stop.
    Applied { spread_pips: f64 },
    /// Widening to `10 × spread` would drop R below `min_r` — the live worker
    /// declines the entry, so the sim/replay must too.
    Rejected,
}

/// Apply the System-1 entry SL-spread floor to `resolved` **in place**, off the
/// fire bar (`candles.first()`) — the single source of the placed stop.
///
/// This is the mirror of `run_enter`'s widen-then-reject: when the signed stop
/// sits closer than `10 × spread` to entry, the worker widens it to `10 × spread`
/// (entering with the wider stop if R still clears `min_r`, declining otherwise).
/// Both `simulate_fill` (the exit sim) and `widened_stop_at` (the System-2
/// baseline) call this so the placed stop, the simulated exit, and the System-2
/// "from" level are all the **same** floored number — they can't drift into the
/// three-different-SLs confusion the journal showed on EUR/AUD
/// `hs-eur-aud-3d0b5dda`. Returns the spread used (for display) or a reject.
///
/// No fire bar (empty path) ⇒ `Applied { spread_pips: 0.0 }` — nothing to floor.
pub fn apply_entry_spread_floor(
    resolved: &mut Resolved,
    pip_size: f64,
    candles: &[BidAskCandle],
    entry_spread_price: Option<f64>,
) -> EntryFloor {
    // The spread the floor sizes off. Prefer a caller-supplied
    // `entry_spread_price` — the MEAN over the trailing spread window, computed
    // once by the caller via the shared `get_bidask_candles` provider +
    // `trailing_spread_mean` (so worker and replay agree). Fall back to the fire
    // bar's own close spread (`candles.first()`) when the caller has no window —
    // the pre-window single-sample behaviour every existing test relies on.
    let spread_price = match entry_spread_price {
        Some(s) => s,
        None => {
            let Some(fire) = candles.first() else {
                return EntryFloor::Applied { spread_pips: 0.0 };
            };
            fire.close_spread()
        }
    };
    match widen_sl_to_spread_floor(
        resolved.entry.reference_price(),
        resolved.stop_loss,
        resolved.take_profit,
        spread_price,
        resolved.min_r,
    ) {
        SlWiden::Unchanged => {}
        SlWiden::Widened { new_stop_loss, .. } => {
            resolved.stop_loss = new_stop_loss;
        }
        SlWiden::Reject { .. } => return EntryFloor::Rejected,
    }
    let spread_pips = if pip_size > 0.0 && pip_size.is_finite() {
        spread_price / pip_size
    } else {
        f64::NAN
    };
    EntryFloor::Applied { spread_pips }
}

/// A System-2 spread-widen the replay reconstructs from the candle path: the
/// bar whose spread tripped the widen, the new (widened) stop level, and — since
/// the widen is *transient* live — the bar at which the recovery watcher would
/// restore the original stop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpreadWiden {
    /// The **sub-candle instant** the live cron would fire the widen — the 30-min
    /// lead moment before a flagged hour (e.g. 20:30Z ahead of a 21:00Z spike), or
    /// the bar open when the bar's own hour is already flagged. This is the
    /// journal-display time; use [`SpreadWiden::effective_from`] for the bar the
    /// widened stop first governs the exit on.
    pub at: chrono::DateTime<chrono::Utc>,
    /// Open-time of the candle the widen fired on — the bar from which the
    /// widened stop governs the exit simulation, up to (but excluding)
    /// [`restored_at`](Self::restored_at). Unlike [`at`](Self::at) (a sub-candle
    /// instant), this is a bar boundary so the exit loop can compare it to each
    /// candle's `time`. The exit sim applies the widened stop on bars in
    /// `[effective_from, restored_at)`; outside that window the pre-widen stop
    /// (original / break-even) applies — matching the live broker amend+restore.
    pub effective_from: chrono::DateTime<chrono::Utc>,
    /// The stop the open position actually carried before this widen — i.e. the
    /// resolved stop **after** the System-1 entry spread floor (the same number
    /// the `order:` line and `simulate_fill` place at). System 2 widens from and
    /// restores to THIS level, so the journal's widen/restore lines reconcile
    /// with the placed stop instead of the un-floored signed level.
    pub original_stop: f64,
    /// Pips crossed on the widen bar (`ask_c − bid_c`), for the journal display.
    pub widen_spread_pips: f64,
    /// The stop after widening away from price (the shared
    /// [`trade_control_core::blackout_widen::widened_stop`] result).
    pub widened_stop: f64,
    /// Open-time of the bar at which the live recovery watcher
    /// (`blackout_watch::watch_recovery`) would restore the original stop:
    /// the first post-widen bar whose spread has dropped to/under the recovered
    /// cutoff (4 pips), or — if the spread stays elevated — the 3-hour backstop.
    /// `None` when neither happens before the position exits or the window ends
    /// (the widen would still be active at exit). The widen is a **transient**
    /// shield, not a permanent risk change — this field is what makes the replay
    /// journal say so.
    pub restored_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Reconstruct a System-2 spread-blackout stop widen from the candle path, if
/// one would apply while this enter's position is open.
///
/// In production the live cron (`src/cron/blackout_apply.rs`) samples
/// `ask − bid`, and when it's inside the spread-blackout window it widens the
/// broker stop *away* from price by the live spread (floored/clamped 22–40 pips
/// via [`trade_control_core::blackout_widen::clamp_widen`] /
/// [`trade_control_core::blackout_widen::widened_stop`]). A widened stop changes
/// the **exit price** — without this, a replay would stop the position out at
/// the original (tighter) level and diverge from the live worker.
///
/// **The per-instrument spread-hour gate (2026-07-05).** The live cron widens
/// at each instrument's *own* learned spread hours (from the candle-derived
/// baseline table), not one global NY-close hour — Gold overnight, EUR/USD at
/// 21:00, indices at their own — via
/// [`trade_control_core::spread_blackout::spread_hour_widen_instant`]. This
/// replay mirrors that: a bar qualifies when `spread_hour_widen_instant(
/// instrument, c.time, bar_seconds)` is `Some` (in/leading into a learned
/// spread hour → widen by the baked p90), OR — for an **uncatalogued**
/// instrument with no learned hours — the bar is at the legacy NY-close edge
/// ([`trade_control_core::ny_clock::is_ny_close_edge`], 21:00 UTC EDT / 22:00
/// EST) AND its live spread reaches `widen_trigger_pips`. The pre-2026-07-05
/// behaviour (global NY-close gate + `clamp_widen`) survives verbatim on the
/// fallback path so uncatalogued assets don't regress.
///
/// **The trigger / widen size.** For a baked spread-hour bar the widen is
/// [`trade_control_core::blackout_widen::spread_hour_widen_size`] — baked p90
/// primary, live spread as a floor, per-instrument ceiling (see that fn's
/// docs). For the legacy fallback the caller's `widen_trigger_pips` (the
/// instrument's `baked-baseline × 5` from
/// [`trade_control_core::spread_blackout::elevated_threshold_pips`], the same
/// number System 1 uses) still gates, and the amount is the flat 22–40
/// [`trade_control_core::blackout_widen::clamp_widen`].
///
/// Pure and side-effect-free. Returns `None` when the enter has no fill, exits
/// before any qualifying spread bar, or no NY-close-edge bar's spread reaches
/// the trigger. The bare (intent-resolving) form is exercised only by this
/// module's tests — production uses the `_resolved` variant — so it is
/// `#[cfg(test)]`.
#[cfg(test)]
fn widened_stop_at(
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
    widen_trigger_pips: f64,
    entry_spread_price: Option<f64>,
) -> Option<SpreadWiden> {
    if !pip_size.is_finite() || pip_size <= 0.0 {
        return None;
    }
    let mut resolved =
        Resolved::from_intent(intent, shell, pip_size, replay_tick(intent, pip_size)).ok()?;
    // Floor the baseline to the placed stop (System 1) so System 2 widens from
    // and restores to the SAME level the order line shows — not the un-floored
    // signed SL. Uses the SAME trailing-window entry spread as the gate/sim
    // (via `entry_spread_price`) so all three floor to one number. A reject
    // means the live worker declined the entry, so there's no position to widen.
    if let EntryFloor::Rejected =
        apply_entry_spread_floor(&mut resolved, pip_size, candles, entry_spread_price)
    {
        return None;
    }
    widened_stop_at_resolved(
        &resolved,
        intent,
        shell,
        pip_size,
        candles,
        widen_trigger_pips,
    )
}

/// [`widened_stop_at`] over a bracket that has ALREADY been resolved + floored by
/// the caller — the "orders are state" path, where the `ReplayBroker` holds the
/// placed stop (the level the broker actually rests on) and there is nothing to
/// re-floor. The System-2 widen is measured **relative to `resolved.stop_loss`**
/// (the placed stop), so feeding the stored bracket here reconstructs the exact
/// widen the live broker holds off that same placed level. The floor-front
/// (`Resolved::from_intent` + `apply_entry_spread_floor`) version above is kept
/// for the report's display path, which resolves from the intent.
pub fn widened_stop_at_resolved(
    resolved: &Resolved,
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
    widen_trigger_pips: f64,
) -> Option<SpreadWiden> {
    widen_episodes_at_resolved(
        resolved,
        intent,
        shell,
        pip_size,
        candles,
        widen_trigger_pips,
    )
    .into_iter()
    .next()
}

/// **Every** widen→restore episode the open position lives through, in order.
///
/// [`widened_stop_at_resolved`] is this function's first element, kept for the
/// journal line that reports "the widen" — but scoring an exit off that single
/// episode is the bug this function exists to fix. A position open for days
/// crosses several spread hours, and the live cron widens at each of them: it
/// clears its `applied` record on restore (`blackout_watch`), which frees the
/// next hour to widen again. This reconstruction used to `return` after the
/// first and leave every later spread hour bare.
///
/// Measured cost of the old shape on AUD/NZD 2026-06-11: the second spread hour
/// (06-14T21:00Z, 18-pip spread) went unshielded, `bid_l = 1.20645` took out the
/// narrow 1.20733 stop for −1.00R, and the widened level 1.20552 was never
/// touched by any bar in the window — the trade ran to TP for +1.18R with it.
///
/// Feeds [`trade_control_core::order_control::WidenEpisodes`], which owns the
/// "which stop is in force on this bar" question for both halves.
pub fn widen_episodes_at_resolved(
    resolved: &Resolved,
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
    widen_trigger_pips: f64,
) -> Vec<SpreadWiden> {
    if !pip_size.is_finite() || pip_size <= 0.0 {
        return Vec::new();
    }
    let dir = resolved.direction;
    let Some(fill) = find_fill(resolved, intent, shell, dir, candles) else {
        return Vec::new();
    };
    let exit_book = book_for(Leg::Exit, dir);

    // The stop IN FORCE, bar by bar — the placement stop until break-even arms,
    // the fill price after (Rule 1). `resolved.stop_loss` alone is the PLACEMENT
    // stop, frozen at placement, and using it as each episode's `original_stop`
    // is the bug this tracking exists to fix: a widen that starts after
    // break-even armed would widen from — and restore to — a level the broker
    // stopped holding hours earlier.
    //
    // Measured on `gbp-zar-h1-2026-07-27`: break-even armed 07-29T01:00 moving
    // the stop to 22.260, a widen fired at 06:30, and the replay widened from
    // (and restored to) 22.350 — ~9 pips wider than live, silently discarding
    // the banked break-even.
    //
    // This is a RULE difference, not a resolution difference. No amount of bar
    // granularity or sub-bar zoom fixes reading the wrong SOURCE number: a finer
    // series would still be measured against the placement stop.
    //
    // The rules and their reasoning live in
    // `core::order_control::in_force_stop`, shared with the live half.
    let mut in_force_stop =
        trade_control_core::order_control::InForceStop::placed_at(resolved.stop_loss);
    let be_arms_at = resolved
        .breakeven
        .map(|be| be.arms_at(fill.entry_price, resolved.take_profit));

    // The bar length, so the System-2 widen can be reported at its exact
    // sub-candle instant (BUG-spread-hour-widen-no-subhour-lead.md). The replay
    // evaluates at bar closes, but the live 15-min cron widens *mid-bar* — 30 min
    // before a flagged hour's top. `spread_hour_widen_instant` returns that precise
    // wall-clock moment (e.g. 20:30Z = 06:30 Brisbane for a 20:00–21:00 bar leading
    // into the 21:00Z spike), so the journal shows the widen where the live worker
    // would actually fire it, not snapped to a bar boundary.
    let bar_seconds = bar_seconds_of(candles);

    // Episodes collected so far. The scan continues past each one — a position
    // open for days crosses several spread hours and the live cron widens at
    // every one of them.
    let mut episodes: Vec<SpreadWiden> = Vec::new();

    for (i, c) in fill.rest.iter().enumerate() {
        // Stop scanning once the position is gone: no later spread hour can
        // shield a position that has already exited.
        //
        // The SL test uses the stop **in force on this bar**, not `original_stop`.
        // Testing the original would end the scan at a level the broker was not
        // actually holding — the widen exists precisely so that touch does not
        // close the trade — and every subsequent episode would be lost. That is
        // the same one-shot failure in a second disguise.
        let in_force = episodes
            .iter()
            .find(|w| c.time >= w.effective_from && w.restored_at.is_none_or(|r| c.time < r))
            .map_or(in_force_stop.level(), |w| w.widened_stop);
        if book_reaches(c, exit_book, in_force, stop_approach(dir))
            || book_reaches(c, exit_book, resolved.take_profit, tp_approach(dir))
        {
            break;
        }
        // One widen per episode: while an earlier widen is still in force, the
        // live cron's `applied` guard refuses to widen again (and would re-capture
        // an already-widened stop as "original" if it did). Only once the restore
        // has cleared that record can the next spread hour widen.
        //
        // A bar inside an active episode is ALSO a bar break-even may not arm off
        // (Rule 2) — which is why the `continue` happens here, before the arming
        // step below, and why the arming step sits after this guard rather than
        // at the top of the loop.
        if episodes
            .last()
            .is_some_and(|w| w.restored_at.is_none_or(|r| c.time < r))
        {
            // Rule 2: a bar inside an active widen does NOT arm break-even, so
            // this `continue` deliberately skips the arming step every other
            // `continue` in this loop performs. That asymmetry IS the rule.
            continue;
        }
        // Rule 1's input, captured BEFORE this bar can arm anything.
        //
        // `original_stop` is what an episode opening on THIS bar widens from and
        // restores to. It must be the stop in force as the bar *opens*, because a
        // bar that starts a widen is itself inside that widen — and Rule 2 says a
        // bar inside a widen does not arm break-even. Reading the level after the
        // arm below would let the widen bar's own close move the stop it is
        // widening from, which is exactly the `covers(effective_from)` case
        // `WidenEpisodes` treats as shielded and `simulate_fill` refuses to arm
        // on. Capturing first is what keeps this scan and `simulate_fill` in
        // agreement about that one bar.
        let original_stop = in_force_stop.level();
        // Per-instrument spread-hour gate — mirror the live cron's System 2
        // (`widen_open_stops_for_spread_hours`). `spread_hour_widen_instant`
        // returns `Some(baked_p90)` iff this bar's instrument is in (or leading
        // into) one of its learned spread hours; `None` means either "not a
        // spread hour now" or "uncatalogued instrument", disambiguated by the
        // legacy `is_ny_close_edge` fallback (so uncatalogued assets keep the old
        // NY-close-only behaviour). Before 2026-07-05 this was a single global
        // `is_ny_close_edge` gate for ALL instruments — see
        // `[[strategy_changes_in_both_replayer_and_worker]]`; the worker + this
        // replay must gate identically.
        // The exact sub-candle instant the live cron would widen for a bar
        // spanning `[c.time, c.time + bar_seconds)` — the 30-min lead instant when
        // this bar LEADS INTO a flagged hour (20:30Z ahead of a 21:00Z spike), or
        // the bar open when the bar's own hour is already flagged. `None` ⇒ this
        // bar neither is nor leads into a spread hour.
        // The candle table gives the widen as a scale-free FRACTION (`spread/mid`);
        // convert to pips with the ORIGINAL STOP as the reference price via
        // `widen_frac_to_pips` — the SAME reference the live cron uses
        // (`blackout_apply::widen_one` passes `original_sl`), so the widened stop
        // this replay reconstructs matches the one the live broker holds to the pip.
        let widen_instant = trade_control_core::spread_blackout::spread_hour_widen_instant(
            &intent.instrument,
            c.time,
            bar_seconds,
        );
        let baked_p90 = widen_instant.map(|(_at, frac)| {
            trade_control_core::spread_blackout::widen_frac_to_pips(frac, original_stop, pip_size)
        });
        if baked_p90.is_none() && !trade_control_core::ny_clock::is_ny_close_edge(c.time) {
            arm_breakeven_on_ordinary_bar(
                &mut in_force_stop,
                resolved,
                be_arms_at,
                dir,
                c.c,
                fill.entry_price,
            );
            continue;
        }
        let spread_pips = (c.ask_c - c.bid_c) / pip_size;
        if !spread_pips.is_finite() {
            arm_breakeven_on_ordinary_bar(
                &mut in_force_stop,
                resolved,
                be_arms_at,
                dir,
                c.c,
                fill.entry_price,
            );
            continue;
        }
        // A baked spread-hour bar widens regardless of the live spread reading
        // (the baked p90 is the primary widen; the timing is what the mask
        // asserts). The legacy fallback still requires the live spread to reach
        // the per-instrument trigger, matching the pre-2026-07-05 behaviour.
        let widen_pips = match baked_p90 {
            Some(p90) => {
                trade_control_core::blackout_widen::spread_hour_widen_size(p90, spread_pips)
            }
            None if spread_pips >= widen_trigger_pips => {
                trade_control_core::blackout_widen::clamp_widen(spread_pips)
            }
            None => {
                arm_breakeven_on_ordinary_bar(
                    &mut in_force_stop,
                    resolved,
                    be_arms_at,
                    dir,
                    c.c,
                    fill.entry_price,
                );
                continue;
            }
        };
        let widened = trade_control_core::blackout_widen::widened_stop(
            dir,
            original_stop,
            widen_pips,
            pip_size,
        );
        // Report the widen at its exact sub-candle instant on the baked path (the
        // 30-min lead moment the live cron fires — 06:30, not the 06:00 bar open);
        // the legacy NY-close-edge fallback has no mask instant, so it stays at the
        // bar open. Restore detection walks the *following bars*, so it's still
        // anchored to the bar time.
        let widen_at = widen_instant.map(|(at, _frac)| at).unwrap_or(c.time);
        let restored_at = restore_bar(&fill.rest[i + 1..], c.time, pip_size, &intent.instrument);
        // This bar opens an episode, so under Rule 2 it does NOT arm break-even
        // — control reaches `episodes.push` below and skips the arm at the foot
        // of the loop via `continue`. Every path that reached here and did NOT
        // open an episode falls through to that arm instead.
        episodes.push(SpreadWiden {
            at: widen_at,
            effective_from: c.time,
            original_stop,
            widen_spread_pips: spread_pips,
            widened_stop: widened,
            restored_at,
        });
        continue;
    }
    episodes
}

/// Arm break-even off an ordinary bar during the episode scan.
///
/// Split out so the scan's several `continue` paths — "not a spread hour", "the
/// live spread never reached the trigger" — all reach the SAME arming step
/// rather than each needing their own copy. Those bars are ordinary: nothing
/// about them being examined by the widen scan makes them un-armable, and an
/// earlier draft that armed only on the fall-through path silently dropped every
/// break-even that would have armed on a non-spread-hour bar.
fn arm_breakeven_on_ordinary_bar(
    in_force_stop: &mut trade_control_core::order_control::InForceStop,
    resolved: &Resolved,
    be_arms_at: Option<f64>,
    dir: trade_control_core::intent::Direction,
    close_price: f64,
    entry_price: f64,
) {
    if let (Some(be), Some(level)) = (resolved.breakeven, be_arms_at) {
        in_force_stop.consider_arm(
            // The caller has already established this bar is not inside an
            // active episode, so the gate is `Armable` by construction. It is
            // passed explicitly rather than skipping `consider_arm`'s gate
            // parameter so the one arming API stays the same on both sides.
            trade_control_core::order_control::BreakevenArmGate::Armable,
            be,
            dir,
            level,
            close_price,
            entry_price,
        );
    }
}

/// When the live recovery watcher (`blackout_watch::watch_recovery`) would
/// restore the widened stop, reconstructed from the post-widen candle path.
///
/// Mirrors the live restore triggers in `blackout_watch::watch_one`: the first
/// bar whose spread has dropped to/under the recovered cutoff
/// (`SPREAD_BLACKOUT_RECOVERED_PIPS`, 4 pips) — clock-agnostic, so recovery is
/// NOT gated on the NY-close edge — or, failing that, the safety force-restore
/// ceiling (`SAFETY_FORCE_RESTORE_SECONDS`, 12h; the last-resort timer, no longer
/// the per-record TTL after the 2026-07 backstop split), whichever comes first.
/// `bars` are the candles strictly after the widen bar; `widen_at` is the widen
/// bar's open time. `None` when neither trigger lands within the provided path
/// (the widen is still active at the window's end).
fn restore_bar(
    bars: &[BidAskCandle],
    widen_at: chrono::DateTime<chrono::Utc>,
    pip_size: f64,
    instrument: &str,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let recovered_cutoff = trade_control_core::spread_blackout::SPREAD_BLACKOUT_RECOVERED_PIPS;
    let safety_secs = trade_control_core::spread_blackout::SAFETY_FORCE_RESTORE_SECONDS;
    let safety_at = widen_at + chrono::Duration::seconds(safety_secs as i64);
    for c in bars {
        // Recovery first (the normal path): the spread dropping back to normal.
        let spread_pips = (c.ask_c - c.bid_c) / pip_size;
        if spread_pips.is_finite() && spread_pips <= recovered_cutoff {
            return Some(c.time);
        }
        // Safety force-restore: at/after the 12h ceiling AND not inside a spread
        // hour. The clock half alone is not enough — see
        // `order_control::backstop_restore_allowed`, the same gate the live call
        // site applies. Without it a weekend gap (12h of wall-clock, zero bars of
        // market) let the backstop restore the narrow stop onto the NY-close spike
        // itself: AUD/NZD 2026-06-11 booked −1.00R on a trade whose widened stop
        // was never touched and which ran to TP for +1.18R.
        if trade_control_core::order_control::backstop_restore_allowed(
            instrument,
            c.time,
            c.time >= safety_at,
        ) {
            return Some(c.time);
        }
    }
    None
}

/// Which broker book a price level is tested against.
#[derive(Clone, Copy, PartialEq)]
enum Book {
    /// A sell fills here (short entry, long exit).
    Bid,
    /// A buy fills here (long entry, short exit).
    Ask,
}

/// Which leg of the trade a level belongs to — entry (open) vs SL/TP (close).
#[derive(Clone, Copy)]
enum Leg {
    Entry,
    Exit,
}

/// The broker book the given leg fills on: a buy uses the ask, a sell uses the
/// bid. Entry side is the trade direction; exit side is its opposite (you close
/// by trading the other way).
fn book_for(leg: Leg, dir: Direction) -> Book {
    match (leg, dir) {
        (Leg::Entry, Direction::Long) => Book::Ask, // buy to open
        (Leg::Entry, Direction::Short) => Book::Bid, // sell to open
        (Leg::Exit, Direction::Long) => Book::Bid,  // sell to close
        (Leg::Exit, Direction::Short) => Book::Ask, // buy to close
    }
}

/// The side a **stop-loss** is approached from for the given trade direction: a
/// long's SL sits below (price falls into it → `FromAbove`); a short's sits above
/// (`FromBelow`).
fn stop_approach(dir: Direction) -> Approach {
    match dir {
        Direction::Long => Approach::FromAbove,
        Direction::Short => Approach::FromBelow,
    }
}

/// The side a **take-profit** is approached from — the mirror of the stop: a
/// long's TP is above (price rises into it → `FromBelow`), a short's below.
fn tp_approach(dir: Direction) -> Approach {
    match dir {
        Direction::Long => Approach::FromBelow,
        Direction::Short => Approach::FromAbove,
    }
}

/// Which side price approaches a level from — the load-bearing distinction for a
/// *touch* (a triggered stop/limit, an SL/TP hit) versus mere *containment*.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Approach {
    /// Price rises to the level: touched when the bar's high reaches it.
    /// A long-stop entry, a long's take-profit, a short's stop-loss.
    FromBelow,
    /// Price falls to the level: touched when the bar's low reaches it.
    /// A short-stop entry, a short's take-profit, a long's stop-loss.
    FromAbove,
}

/// Whether the candle's chosen book range **reaches** `level`, approaching from
/// the given side. This is a *directional touch*, not containment: a bar that
/// gaps or opens already past the level (its whole range on the far side) still
/// counts, because price traded through the level to get there.
///
/// `FromBelow` ⇒ `high >= level` (an ascending order/target is hit the moment the
/// high reaches it, even if the low never dips back below). `FromAbove` ⇒
/// `low <= level`. The old bracket test (`lo <= level <= hi`) silently *missed*
/// the gap-through case — an up-gap through a long-stop trigger left every
/// post-fire bar's low above the trigger, so the order was reported NeverFilled
/// even though a real broker stop fills on the gap. The *book* (bid vs ask) still
/// carries the real per-bar spread. See BUG-replay-stop-fill-gap.
fn book_reaches(c: &BidAskCandle, book: Book, level: f64, approach: Approach) -> bool {
    let (lo, hi) = match book {
        Book::Bid => (c.bid_l, c.bid_h),
        Book::Ask => (c.ask_l, c.ask_h),
    };
    match approach {
        Approach::FromBelow => hi >= level,
        Approach::FromAbove => lo <= level,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use trade_control_core::intent::{
        Action, BrokerKind, EntrySpec, PriceAnchor, PriceRef, TakeProfit,
    };
    use trade_control_core::tunable::Tunable;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// A bar with **bid == ask == mid** (zero spread) — the data-source-serves-
    /// mid-only case. Lets the level-logic tests read as plain OHLC while
    /// exercising the bid/ask code paths through the degenerate (zero-spread)
    /// branch.
    fn candle(time: &str, o: f64, h: f64, l: f64, c: f64) -> BidAskCandle {
        BidAskCandle {
            time: ts(time),
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

    /// A bar with explicit bid/ask books (mid is their midpoint, unused by the
    /// fill test). For the spread-specific tests: `bid_*` is the sell book,
    /// `ask_*` the buy book.
    #[allow(clippy::too_many_arguments)]
    fn ba_candle(time: &str, bid_h: f64, bid_l: f64, ask_h: f64, ask_l: f64) -> BidAskCandle {
        BidAskCandle {
            time: ts(time),
            o: (bid_l + ask_h) / 2.0,
            h: (bid_h + ask_h) / 2.0,
            l: (bid_l + ask_l) / 2.0,
            c: (bid_l + ask_h) / 2.0,
            bid_o: bid_l,
            bid_h,
            bid_l,
            bid_c: bid_l,
            ask_o: ask_h,
            ask_h,
            ask_l,
            ask_c: ask_h,
        }
    }

    /// A long stop-entry intent with SL `1.1000` and TP `1.1150` as absolute
    /// levels, so the test doesn't depend on anchor resolution. The entry's stop
    /// trigger is overridden per-test.
    fn long_stop_intent() -> Intent {
        let mut i = base_enter();
        i.direction = Some(Direction::Long);
        i.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 0.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        i.stop_loss = Some(PriceRef::Absolute { absolute: 1.1000 });
        i.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.1150,
        }));
        i
    }

    #[test]
    fn entry_floor_prefers_supplied_window_spread_over_fire_bar() {
        // A short entry at 1.1000, SL 1.1005 (5 pips above), TP 1.0950. The fire
        // bar is spiky (20-pip spread) but the WINDOWED mean is calm (1 pip).
        let mut intent = base_enter();
        intent.direction = Some(Direction::Short);
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 0.0,
            at: Some(1.1000),
            offset_atr_pct: None,
            recover_entry: None,
        });
        intent.stop_loss = Some(PriceRef::Absolute { absolute: 1.1005 });
        // TP far enough (400 pips) that even the fire-bar 200-pip widen keeps
        // R ≥ 1, so both paths are Applied and the test isolates the *spread
        // source*, not the R-reject.
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.0600,
        }));
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1002, 1.1006, 1.0999, 1.1003).mid(),
        );
        // Fire bar carries a 20-pip spread (bid 1.0990 / ask 1.1010 close).
        let fire = [ba_candle(
            "2026-06-17T10:30:00Z",
            1.0990,
            1.0990,
            1.1010,
            1.1010,
        )];

        // Fire-bar path (entry_spread_price = None) → floor off 0.0020 → widen to
        // 10× = 0.0200 above entry → SL 1.1200.
        let mut r_fire = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).expect("resolves");
        let out_fire = apply_entry_spread_floor(&mut r_fire, 0.0001, &fire, None);
        assert!(
            matches!(out_fire, EntryFloor::Applied { .. }),
            "{out_fire:?}"
        );
        assert!(
            (r_fire.stop_loss - 1.1200).abs() < 1e-9,
            "fire-bar spread widens to 1.1200, got {}",
            r_fire.stop_loss
        );

        // Windowed path (entry_spread_price = Some(1 pip)) → floor off 0.0001 →
        // 10× = 0.0010 above entry → SL 1.1010, far tighter. The SAME fire slice,
        // only the supplied spread differs — proving the window wins.
        let mut r_win = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).expect("resolves");
        let out_win = apply_entry_spread_floor(&mut r_win, 0.0001, &fire, Some(0.0001));
        assert!(matches!(out_win, EntryFloor::Applied { .. }), "{out_win:?}");
        assert!(
            (r_win.stop_loss - 1.1010).abs() < 1e-9,
            "windowed spread widens to 1.1010, got {}",
            r_win.stop_loss
        );
    }

    fn base_enter() -> Intent {
        Intent {
            entry_level_vetos: Vec::new(),
            v: 1,
            id: "sim-test".into(),
            not_before: None,
            not_after: ts("2026-06-30T00:00:00Z"),
            action: Action::Enter,
            instrument: "EUR_USD".into(),
            direction: None,
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
            trade_id: Some("sim-test".into()),
            max_retries: Tunable::Static(0),
            entry_dedup: None,
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
            contract_multiplier: None,
            spread_window: None,
            trade_plan: None,
            blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
            breakeven: None,
            include_archived: false,
        }
    }

    /// The shell the fire carried: a trigger candle closing at 1.1040 (below the
    /// 1.1050 stop, so the stop is valid — long stop sits above close).
    fn trigger_shell() -> Shell {
        Shell::from_candle(&candle("2026-06-17T10:00:00Z", 1.1035, 1.1045, 1.1030, 1.1040).mid())
    }

    /// The **fire bar** — `candles[0]` in production, the bar the enter fired on.
    /// A pending order isn't live until this bar closes, so `simulate_fill` skips
    /// it for the fill search. Tests prepend this so the path matches production's
    /// `fire.forward` shape (fire bar first, then the post-fire path). Its range
    /// (1.1041–1.1045) deliberately misses every per-test trigger/SL/TP so its
    /// only role is to be skipped.
    fn fire_bar() -> BidAskCandle {
        candle("2026-06-17T10:30:00Z", 1.1042, 1.1045, 1.1041, 1.1043)
    }

    #[test]
    fn stop_entry_fills_then_takes_profit() {
        let intent = long_stop_intent();
        let shell = trigger_shell();
        // Stop trigger = close (1.1040) + 0 pips → wait, EntrySpec::Stop anchors
        // to `from`=Close=1.1040; long stop must be ABOVE close, so resolver
        // requires trigger > close. With offset 0 that's invalid; use a candle
        // path where the fill candle reaches the resolved trigger.
        // Resolver: trigger = anchor(Close)=1.1040; long-stop-<=close errors.
        // So this intent resolves with InvalidGeometry — assert that path first.
        let outcome = simulate_fill(&intent, &shell, 0.0001, &[]);
        assert!(
            matches!(outcome, SimOutcome::Unresolved(_)),
            "0-offset long stop on close is invalid geometry: {outcome:?}"
        );
    }

    #[test]
    fn offset_stop_fills_then_tp_then_sl_paths() {
        // Stop 10 pips above close → trigger 1.1050. SL 1.1000, TP 1.1150.
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();

        // Path A: a candle reaches 1.1050 (fills), a later one reaches 1.1150 (TP).
        // Each path leads with the fire bar (skipped — order isn't live until it
        // closes), so the first *fillable* bar is index 1.
        let tp_path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills @1.1050
            candle("2026-06-17T12:00:00Z", 1.1052, 1.1160, 1.1050, 1.1155), // hits TP 1.1150
        ];
        match simulate_fill(&intent, &shell, 0.0001, &tp_path) {
            SimOutcome::TookProfit {
                entry_price,
                exit_price,
                ..
            } => {
                assert!((entry_price - 1.1050).abs() < 1e-9);
                assert!((exit_price - 1.1150).abs() < 1e-9);
            }
            other => panic!("expected TookProfit, got {other:?}"),
        }

        // Path B: fills, then a candle reaches the SL 1.1000.
        let sl_path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills
            candle("2026-06-17T12:00:00Z", 1.1050, 1.1051, 1.0995, 1.1000), // hits SL 1.1000
        ];
        match simulate_fill(&intent, &shell, 0.0001, &sl_path) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!((exit_price - 1.1000).abs() < 1e-9);
            }
            other => panic!("expected StoppedOut, got {other:?}"),
        }

        // Path C: never reaches the trigger → NeverFilled.
        let no_fill = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043),
        ];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &no_fill),
            SimOutcome::NeverFilled
        );

        // Path D: fills but neither level touched → FilledOpen.
        let still_open = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills
            candle("2026-06-17T12:00:00Z", 1.1052, 1.1060, 1.1048, 1.1055), // neither
        ];
        assert!(matches!(
            simulate_fill(&intent, &shell, 0.0001, &still_open),
            SimOutcome::FilledOpen { .. }
        ));
    }

    /// Regression for BUG-replay-stop-fill-gap (AUD/NZD iH&S long 2026-07-06):
    /// price **gapped up through** the long-stop trigger, so every post-fire bar
    /// OPENED already above it — the bar's low never dipped back to the trigger.
    /// The old bracket fill test (`lo <= trigger <= hi`) reported NeverFilled even
    /// though a real broker stop fills on the gap. `book_reaches`/`FromBelow`
    /// (`hi >= trigger`) fills it.
    #[test]
    fn long_stop_fills_when_price_gaps_up_through_trigger() {
        // Long stop 10 pips above close → trigger 1.1050.
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();

        // The fillable bar OPENS at 1.1052 (already past the 1.1050 trigger) and
        // its whole range 1.1051–1.1060 sits ABOVE the trigger — a gap-through.
        // Old bracket test: low 1.1051 > 1.1050 → missed. New: high 1.1060 >=
        // 1.1050 → fills @ the trigger price.
        let gap_up = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1052, 1.1060, 1.1051, 1.1058), // gaps through
            candle("2026-06-17T12:00:00Z", 1.1058, 1.1160, 1.1056, 1.1155), // hits TP 1.1150
        ];
        match simulate_fill(&intent, &shell, 0.0001, &gap_up) {
            SimOutcome::TookProfit { entry_price, .. } => {
                assert!(
                    (entry_price - 1.1050).abs() < 1e-9,
                    "fills at the stop trigger, not the gap-open price"
                );
            }
            other => panic!("gapped-through long stop must fill, got {other:?}"),
        }
    }

    /// Mirror of the above for a **short** stop: price gaps DOWN through the
    /// trigger, every post-fire bar's high stays below it. `FromAbove`
    /// (`lo <= trigger`) fills it; the old bracket test missed it.
    #[test]
    fn short_stop_fills_when_price_gaps_down_through_trigger() {
        // Absolute sell-stop trigger 1.1000, SL 1.1030 (above), TP 1.0950 (below).
        let intent = short_stop_intent();
        let shell = trigger_shell();

        // Fillable bar OPENS at 1.0998 (already below the 1.1000 trigger), whole
        // range 1.0990–1.0999 below it — a down-gap. High 1.0999 < 1.1000 so the
        // old bracket test missed it; low 1.0990 <= 1.1000 fills.
        let gap_down = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.0998, 1.0999, 1.0990, 1.0992), // gaps through
            candle("2026-06-17T12:00:00Z", 1.0992, 1.0994, 1.0945, 1.0948), // hits TP 1.0950
        ];
        match simulate_fill(&intent, &shell, 0.0001, &gap_down) {
            SimOutcome::TookProfit { entry_price, .. } => {
                assert!(
                    (entry_price - 1.1000).abs() < 1e-9,
                    "fills at the short-stop trigger"
                );
            }
            other => panic!("gapped-through short stop must fill, got {other:?}"),
        }
    }

    #[test]
    fn pending_stop_does_not_fill_on_a_spread_hour_bar() {
        // A resting sell-stop (trigger 1.1000) whose price is first reached on a
        // 21:00Z spread-hour bar must NOT fill there — the candle is a rubbish
        // liquidity-vacuum spike. The order stays resting and fills on the next
        // CLEAN bar (23:00Z), then takes profit. Mirrors the AUD/CHF 2026-07-08
        // fill-into-the-spread-hour case that motivated this. `EUR_USD` is
        // un-sampled, so `is_spread_hour` uses the 21:00Z NY-close-edge fallback.
        let intent = short_stop_intent();
        let shell = trigger_shell();
        let path = [
            // fire bar (skipped), well before the spread hour.
            candle("2026-06-17T20:00:00Z", 1.1042, 1.1045, 1.1041, 1.1043),
            // 21:00Z SPREAD HOUR: range straddles the 1.1000 trigger (low 1.0990)
            // — WOULD fill, but is rubbish → skipped.
            candle("2026-06-17T21:00:00Z", 1.0998, 1.1002, 1.0990, 1.0995),
            // 23:00Z CLEAN: reaches the trigger again → fills here.
            candle("2026-06-17T23:00:00Z", 1.0999, 1.1001, 1.0992, 1.0996),
            // TP bar: runs to 1.0950.
            candle("2026-06-18T00:00:00Z", 1.0994, 1.0996, 1.0945, 1.0948),
        ];
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::TookProfit {
                fill_at,
                entry_price,
                ..
            } => {
                assert_eq!(
                    fill_at,
                    ts("2026-06-17T23:00:00Z"),
                    "fill must skip the 21:00Z spread-hour bar and land on the clean 23:00Z bar"
                );
                assert!((entry_price - 1.1000).abs() < 1e-9);
            }
            other => panic!("expected a clean-bar fill then TP, got {other:?}"),
        }
    }

    #[test]
    fn pending_stop_fills_on_the_same_bar_off_a_spread_hour() {
        // Predicate-false twin: the exact same fill bar on a NON-edge hour fills
        // immediately (byte-identical to today). 12:00Z is not an NY-close edge.
        let intent = short_stop_intent();
        let shell = trigger_shell();
        let path = [
            candle("2026-06-17T10:30:00Z", 1.1042, 1.1045, 1.1041, 1.1043),
            candle("2026-06-17T11:00:00Z", 1.0998, 1.1002, 1.0990, 1.0995),
            candle("2026-06-17T12:00:00Z", 1.0994, 1.0996, 1.0945, 1.0948),
        ];
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::TookProfit { fill_at, .. } => {
                assert_eq!(
                    fill_at,
                    ts("2026-06-17T11:00:00Z"),
                    "a clean-hour bar fills immediately as today"
                );
            }
            other => panic!("expected an immediate fill then TP, got {other:?}"),
        }
    }

    /// Stage 4 — the engine-level payoff of the DST-aware spread-hour mask:
    /// the SAME schedule-LOCAL spread hour (5pm New York) suppresses a
    /// pending-order fill in BOTH summer and winter, at the correspondingly
    /// different UTC hours — and a bar one UTC hour off in each season is NOT
    /// suppressed. This proves the engine keys on the local-hour mask, not a
    /// fixed UTC hour.
    ///
    /// `AUD_CHF` (oanda, schedule `ny`, mask `1<<17`) is a real row in the baked
    /// candle table with the 17:00-local bit set.
    ///
    /// **Why replay == live.** The fill loop in `find_fill` skips a spread-hour
    /// bar via `trade_control_core::spread_blackout::suppress_on_spread_hour_bar_seconds`
    /// (see the call at the top of this file). The live worker's entry
    /// suppression consumes that *same* core seam. So an engine-level assertion
    /// on that seam — for the four (summer-hit, winter-hit, summer-miss,
    /// winter-miss) instants — is a faithful proof that the replay engine and the
    /// live worker inherit identical DST-correct behaviour. We assert on the seam
    /// directly (the exact call the simulator makes) AND drive the full
    /// `simulate_fill` path for the summer/winter hits to prove the fill actually
    /// skips the spike bar.
    #[test]
    fn spread_hour_suppression_is_dst_invariant() {
        use trade_control_core::spread_blackout::suppress_on_spread_hour_bar_seconds;

        // The engine derives `bar_seconds` from consecutive candle spacing; H1 is
        // 3600s, ≤ the max short-bar threshold, so the suppression reduces to the
        // pure DST-aware `is_spread_hour` mask lookup.
        const H1: i64 = 3600;

        // Summer (EDT, UTC-4): 2026-07-09 17:00 New York == 21:00 UTC.
        let summer_hit = ts("2026-07-09T21:00:00Z");
        // Winter (EST, UTC-5): 2026-01-15 17:00 New York == 22:00 UTC.
        let winter_hit = ts("2026-01-15T22:00:00Z");
        // Negatives: one UTC hour off in each season is a DIFFERENT local hour.
        // 22:00 UTC in summer = 18:00 EDT (clean); 21:00 UTC in winter = 16:00
        // EST (clean). A fixed-UTC bug would flag the wrong one of these.
        let summer_miss = ts("2026-07-09T22:00:00Z");
        let winter_miss = ts("2026-01-15T21:00:00Z");

        // Seam parity: the exact call the fill loop makes, for all four instants.
        assert!(
            suppress_on_spread_hour_bar_seconds("AUD_CHF", summer_hit, H1),
            "17:00 EDT (21:00 UTC) must suppress the fill"
        );
        assert!(
            suppress_on_spread_hour_bar_seconds("AUD_CHF", winter_hit, H1),
            "17:00 EST (22:00 UTC) must suppress the fill — SAME local hour"
        );
        assert!(
            !suppress_on_spread_hour_bar_seconds("AUD_CHF", summer_miss, H1),
            "18:00 EDT (22:00 UTC) is NOT the 17:00 spike — must not suppress"
        );
        assert!(
            !suppress_on_spread_hour_bar_seconds("AUD_CHF", winter_miss, H1),
            "16:00 EST (21:00 UTC) is NOT the 17:00 spike — must not suppress"
        );

        // Full fill-path proof: a resting sell-stop reaches its 1.1000 trigger on
        // the spread-hour spike bar but does NOT fill there — it fills on the next
        // clean bar, then takes profit. Mirrors `pending_stop_does_not_fill_on_a_
        // spread_hour_bar` but on the *table-driven* `AUD_CHF` mask, at the two
        // DST-shifted UTC hours.
        let mut intent = short_stop_intent();
        intent.instrument = "AUD_CHF".into();
        let shell = trigger_shell();

        // Summer: spike at 21:00Z (17:00 EDT), clean fill at 23:00Z.
        let summer_path = [
            candle("2026-07-09T20:00:00Z", 1.1042, 1.1045, 1.1041, 1.1043), // fire (skipped)
            candle("2026-07-09T21:00:00Z", 1.0998, 1.1002, 1.0990, 1.0995), // spike → skipped
            candle("2026-07-09T23:00:00Z", 1.0999, 1.1001, 1.0992, 1.0996), // clean → fills
            candle("2026-07-10T00:00:00Z", 1.0994, 1.0996, 1.0945, 1.0948), // TP 1.0950
        ];
        match simulate_fill(&intent, &shell, 0.0001, &summer_path) {
            SimOutcome::TookProfit { fill_at, .. } => assert_eq!(
                fill_at,
                ts("2026-07-09T23:00:00Z"),
                "summer fill must skip the 21:00Z (17:00 EDT) spike bar"
            ),
            other => panic!("summer: expected clean-bar fill then TP, got {other:?}"),
        }

        // Winter: spike shifts to 22:00Z (17:00 EST), clean fill at 23:00Z. Same
        // LOCAL spread hour, one UTC hour later — the DST-invariance payoff.
        let winter_path = [
            candle("2026-01-15T21:00:00Z", 1.1042, 1.1045, 1.1041, 1.1043), // fire (skipped)
            candle("2026-01-15T22:00:00Z", 1.0998, 1.1002, 1.0990, 1.0995), // spike → skipped
            candle("2026-01-15T23:00:00Z", 1.0999, 1.1001, 1.0992, 1.0996), // clean → fills
            candle("2026-01-16T00:00:00Z", 1.0994, 1.0996, 1.0945, 1.0948), // TP 1.0950
        ];
        match simulate_fill(&intent, &shell, 0.0001, &winter_path) {
            SimOutcome::TookProfit { fill_at, .. } => assert_eq!(
                fill_at,
                ts("2026-01-15T23:00:00Z"),
                "winter fill must skip the 22:00Z (17:00 EST) spike bar"
            ),
            other => panic!("winter: expected clean-bar fill then TP, got {other:?}"),
        }
    }

    /// A long whose stop-loss is **gapped through** (bar opens already below the
    /// SL, low and high both under it) must still stop out — the same gap bug on
    /// the exit leg. `stop_approach(Long)` = `FromAbove` (`lo <= sl`) catches it.
    #[test]
    fn long_stops_out_when_price_gaps_down_through_sl() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();
        // Fills @1.1050, then a bar GAPS DOWN entirely below the SL 1.1000
        // (range 1.0980–1.0990, both under 1.1000). Bracket test would miss it.
        let gap_sl = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills @1.1050
            candle("2026-06-17T12:00:00Z", 1.0990, 1.0990, 1.0980, 1.0985), // gaps below SL
        ];
        match simulate_fill(&intent, &shell, 0.0001, &gap_sl) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!((exit_price - 1.1000).abs() < 1e-9);
            }
            other => panic!("gapped-through SL must stop out, got {other:?}"),
        }
    }

    #[test]
    fn entry_level_veto_flips_loss_to_no_fill() {
        // Bug #12 regression — the −110.53 GBP path. Same candles, same
        // resolved entry: with NO entry-level veto the order fills and runs to
        // its stop (a loss); with a breached pcl-exhausted level baked on, the
        // simulator declines before filling (£0), exactly as the worker's
        // `run_enter` gate would.
        use trade_control_core::intent::{EntryLevelVeto, VetoSide};
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();
        // Fills @1.1050, then a candle reaches SL 1.1000 → StoppedOut. Lead with
        // the fire bar (skipped) so the fill lands on the realistic index 1.
        let sl_path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052),
            candle("2026-06-17T12:00:00Z", 1.1050, 1.1051, 1.0995, 1.1000),
        ];

        // Baseline: no veto → the loss path.
        assert!(matches!(
            simulate_fill(&intent, &shell, 0.0001, &sl_path),
            SimOutcome::StoppedOut { .. }
        ));

        // With a pcl-exhausted level the entry (1.1050) is already past
        // (`Above` 1.1040 for a long) → declined, no fill.
        intent.entry_level_vetos = vec![EntryLevelVeto {
            name: "too-high".into(),
            level: 1.1040,
            past: VetoSide::Above,
        }];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &sl_path),
            SimOutcome::Declined {
                name: "too-high".into()
            }
        );

        // An entry short of the level still fills (don't-over-decline control).
        intent.entry_level_vetos = vec![EntryLevelVeto {
            name: "too-high".into(),
            level: 1.1060, // entry 1.1050 < 1.1060 → not past
            past: VetoSide::Above,
        }];
        assert!(matches!(
            simulate_fill(&intent, &shell, 0.0001, &sl_path),
            SimOutcome::StoppedOut { .. }
        ));
    }

    /// An NY-close-edge UTC instant — 21:00 UTC on an EDT day (12-Mar-2026,
    /// past the 2nd Sunday of March). `is_ny_close_edge` is true here, so the
    /// offline blackout window stand-in is "open". Used to fire enters inside
    /// the trough.
    const EDGE_TS: &str = "2026-03-12T21:00:00Z";
    /// A non-edge UTC instant — 10:00 UTC, mid-London-session, window closed.
    const NON_EDGE_TS: &str = "2026-03-12T10:00:00Z";

    /// A fire bar with an explicit `ask_c − bid_c` spread, in PRICE units, at
    /// `time`. Only the close books carry the spread (the worker samples a quote
    /// close ≈ the bar close); the rest are filled in arbitrarily but
    /// consistently so the bar is well-formed and never crosses any per-test
    /// level.
    fn spread_fire_bar(time: &str, mid: f64, spread_price: f64) -> BidAskCandle {
        let half = spread_price / 2.0;
        BidAskCandle {
            time: ts(time),
            o: mid,
            h: mid + 0.0001,
            l: mid - 0.0001,
            c: mid,
            bid_o: mid - half,
            bid_h: mid - half + 0.0001,
            bid_l: mid - half - 0.0001,
            bid_c: mid - half,
            ask_o: mid + half,
            ask_h: mid + half + 0.0001,
            ask_l: mid + half - 0.0001,
            ask_c: mid + half,
        }
    }

    /// A resolvable long stop-entry (trigger 10 pips above the 1.1040 close, so
    /// the geometry is valid and resolution doesn't short-circuit before the
    /// spread gate). The spread tests vary only the fire bar's book + time.
    fn resolvable_long_stop() -> Intent {
        let mut i = long_stop_intent();
        i.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050 > close 1.1040 → valid long stop
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        i
    }

    // NOTE: the System-1 spread-blackout gate is NO LONGER re-derived in
    // `simulate_fill` — the replay driver seeds the store window marker so
    // `run_enter`'s own gate rejects a trough-spread enter pre-placement. The old
    // `elevated_spread_inside_ny_close_edge_is_blacked_out` / `normal_spread_
    // inside_window_fills` tests (which asserted the removed `SimOutcome::
    // SpreadBlackout`) were deleted; blackout rejection is now covered where the
    // gate lives (`core::dispatch::enter` + the replay driver). This surviving
    // control keeps exercising the SL-vs-spread floor for a wide fire-bar spread.

    #[test]
    fn wide_fire_bar_spread_trips_the_sl_floor_decline() {
        // A wide 30-pip fire-bar spread (0.0030 at pip 0.0001): this long stop's
        // SL is 50 pips, so `10 × 30 = 300` pips >> 50 → the SL-vs-spread floor is
        // violated, the widen pushes the stop to 300 pips, and R collapses to
        // 100/300 ≈ 0.33 < 1 → Declined via the widen mirror. (Blackout is no
        // longer a `simulate_fill` concern; this is purely the SL-floor path.)
        let intent = resolvable_long_stop();
        let shell = Shell::from_candle(&spread_fire_bar(NON_EDGE_TS, 1.1040, 0.0030).mid());
        let path = [spread_fire_bar(NON_EDGE_TS, 1.1040, 0.0030)];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::Declined {
                name: "sl-widen-below-min-r".to_string(),
            },
            "the wide spread trips the SL-vs-spread floor; widening to 10x drops R<1 → declined"
        );
    }

    #[test]
    fn widened_sl_protects_the_leg_in_the_fill_path() {
        // The follow-up bug (BUG-sl-spread-floor…, 2026-07-01): the widen was
        // applied to the entry R-check but NOT to the stop the fill/exit sim
        // checks against, so a leg stopped out at the OLD un-widened SL even
        // though the live broker stop sat at the widened level.
        //
        // Geometry: long stop trigger 1.1050, SL 1.1000 (50 pips), TP 1.1150.
        // A 6-pip spread trips the 10× floor (60 > 50) → widen to 10× = 60 pips
        // → SL moves DOWN to 1.0990. R = 100/60 ≈ 1.67 ≥ 1 → entry stands.
        // An adverse bar then dips its bid to 1.0994: that crosses the ORIGINAL
        // 1.1000 SL (the buggy behaviour would stop out here) but NOT the
        // widened 1.0990 — so with the fix the leg survives and stays open.
        let intent = resolvable_long_stop();
        let fire = spread_fire_bar(NON_EDGE_TS, 1.1040, 0.0006); // 6-pip spread
        let shell = Shell::from_candle(&fire.mid());
        // Fill bar: ask reaches the 1.1050 trigger (long fills on the ask book).
        let fill_bar = ba_candle("2026-03-12T11:00:00Z", 1.1052, 1.1045, 1.1055, 1.1048);
        // Adverse bar: bid dips to 1.0994 — past the OLD SL, short of the widened.
        let dip_bar = ba_candle("2026-03-12T12:00:00Z", 1.1010, 1.0994, 1.1015, 1.0998);
        let path = [fire, fill_bar, dip_bar];
        let out = simulate_fill(&intent, &shell, 0.0001, &path);
        assert!(
            matches!(out, SimOutcome::FilledOpen { .. }),
            "leg must survive: the dip crosses the old SL but not the widened one, got {out:?}"
        );
    }

    #[test]
    fn breakeven_armed_at_applies_the_sl_spread_floor() {
        use trade_control_core::intent::Breakeven;
        // Regression for the USD/SGD iH&S replay (2026-07-10): the SL→break-even
        // annotation line went missing even though the leg armed BE and scratched
        // at entry. Root cause — `breakeven_armed_at` walked the path against the
        // *un-floored* signed SL while `simulate_fill_windowed` had widened the SL
        // to the 10× spread floor. A wick dipping between the two levels made
        // `breakeven_armed_at` falsely report "stopped out before arming" → None,
        // so the report suppressed the line while the sim still scratched at BE.
        //
        // Same geometry as `widened_sl_protects_the_leg_in_the_fill_path`: long
        // stop trigger 1.1050, signed SL 1.1000, TP 1.1150; a 6-pip spread trips
        // the 10× floor and widens the SL DOWN to 1.0990. BE 50%-to-TP level =
        // 1.1050 + 0.5×(1.1150−1.1050) = 1.1100.
        let mut intent = resolvable_long_stop();
        intent.breakeven = Some(Breakeven::at_half());
        let fire = spread_fire_bar(NON_EDGE_TS, 1.1040, 0.0006); // 6-pip spread
        let shell = Shell::from_candle(&fire.mid());
        // Fill: ask reaches the 1.1050 trigger.
        let fill_bar = ba_candle("2026-03-12T11:00:00Z", 1.1052, 1.1045, 1.1055, 1.1048);
        // Pre-arming dip: bid low 1.0994 — past the signed 1.1000 SL, short of the
        // widened 1.0990. The real (floored) leg survives; the un-floored walk
        // used to bail out here.
        let dip_bar = ba_candle("2026-03-12T12:00:00Z", 1.1010, 1.0994, 1.1015, 1.0998);
        // Arming bar: mid close ≥ 1.1100 (bid 1.1105 / ask 1.1109 → mid 1.1107).
        let arm_bar = ba_candle("2026-03-12T13:00:00Z", 1.1110, 1.1103, 1.1112, 1.1105);
        let path = [fire, fill_bar, dip_bar, arm_bar];

        // Without the floor the walk bails at `dip_bar` (bid 1.0994 ≤ signed 1.1000).
        // With the floor applied the widened 1.0990 stop holds and BE arms on
        // `arm_bar`.
        let armed = breakeven_armed_at(&intent, &shell, 0.0001, &path, None);
        assert_eq!(
            armed,
            Some(arm_bar.time),
            "BE must arm on the bar past 50%-to-TP; the pre-arming dip crosses the \
             signed SL but not the floored one, so it must not suppress the arm"
        );
    }

    #[test]
    fn mid_only_feed_never_blacks_out() {
        // A mid-only data source has bid == ask == mid → zero spread. Even a
        // fire bar at the close edge must never black out (we don't fabricate a
        // spread the data doesn't carry).
        let intent = resolvable_long_stop();
        let shell = Shell::from_candle(&candle(EDGE_TS, 1.1040, 1.1041, 1.1039, 1.1040).mid());
        let path = [candle(EDGE_TS, 1.1040, 1.1041, 1.1039, 1.1040)];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::NeverFilled,
            "zero-spread mid-only bar must never black out"
        );
    }

    /// Override a short stop-entry intent's entry trigger / SL / TP to absolute
    /// levels, so a BE test controls the exact 50%-to-TP geometry.
    fn i_set_levels(intent: &mut Intent, entry: f64, sl: f64, tp: f64) {
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 0.0,
            offset_atr_pct: None,
            at: Some(entry),
            recover_entry: None,
        });
        intent.stop_loss = Some(PriceRef::Absolute { absolute: sl });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute { absolute: tp }));
    }

    #[test]
    fn breakeven_scratches_a_leg_that_runs_50pct_then_reverses() {
        // Trade-075 Wheat leg-2 shape (BUG-replay-no-breakeven-stop-at-50pct),
        // simplified to round levels but the same geometry: a SHORT that runs
        // past the 50%-to-TP mark on a close, then bounces back to the original
        // SL. Without BE → StoppedOut at the original SL (−1R). With BE → the
        // stop is moved to entry once a candle closes past 50%, so the bounce
        // closes it at break-even (entry), a 0R scratch.
        use trade_control_core::intent::Breakeven;

        // Short stop-entry at 1.1000, original SL 1.1040 (above), TP 1.0900
        // (below). 50%-to-TP level = 1.1000 + 0.5×(1.0900 − 1.1000) = 1.0950.
        let mut intent = short_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.1040, 1.0900);
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );

        // Path: fire bar (skipped) → fill bar reaches the 1.1000 sell-stop on
        // the bid → a candle that CLOSES at 1.0940 (past the 1.0950 BE level,
        // arming BE) → a candle that bounces back up to the 1.1040 ORIGINAL SL.
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1005, 1.1005, 1.0995, 1.1000);
        let runs_past_50 = candle("2026-06-17T12:00:00Z", 1.0990, 1.0992, 1.0935, 1.0940); // closes past 1.0950
        let bounce_to_orig_sl = candle("2026-06-17T13:00:00Z", 1.0945, 1.1041, 1.0944, 1.1000);
        let path = [fire_bar(), fill_bar, runs_past_50, bounce_to_orig_sl];

        // Baseline: NO breakeven → the bounce hits the original 1.1040 SL.
        intent.breakeven = None;
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!(
                    (exit_price - 1.1040).abs() < 1e-9,
                    "no-BE: stopped at the original SL 1.1040, got {exit_price}"
                );
            }
            other => panic!("no-BE: expected StoppedOut at original SL, got {other:?}"),
        }

        // With BE at 50%: the 1.0940 close arms BE (SL → entry 1.1000); the
        // bounce bar (which spans 1.0944..1.1041) now hits the MOVED stop at the
        // entry price 1.1000 first → break-even scratch, not −1R.
        intent.breakeven = Some(Breakeven::at_half());
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut {
                exit_price,
                entry_price,
                ..
            } => {
                assert!(
                    (exit_price - 1.1000).abs() < 1e-9,
                    "BE: stop moved to entry 1.1000, got {exit_price}"
                );
                assert!(
                    (exit_price - entry_price).abs() < 1e-9,
                    "BE: exit == entry → 0R scratch"
                );
            }
            other => panic!("BE: expected break-even stop-out at entry, got {other:?}"),
        }
    }

    /// A long H1 path whose bars each span `range`, walking down from `start` at
    /// `step` per bar. Used to warm the ATR for the noise-floor tests: 40 bars
    /// clears the H1 ATR length (24) with room to spare, and the constant range
    /// makes the resulting ATR a number the test can state.
    fn walk_down(first_time: &str, n: i64, start: f64, step: f64, range: f64) -> Vec<BidAskCandle> {
        let t0 = ts(first_time);
        (0..n)
            .map(|i| {
                let close = start - step * i as f64;
                let mid = |v: f64| v;
                BidAskCandle {
                    time: t0 + chrono::Duration::hours(i),
                    o: mid(close + step),
                    h: mid(close + range / 2.0),
                    l: mid(close - range / 2.0),
                    c: mid(close),
                    bid_o: close + step,
                    bid_h: close + range / 2.0,
                    bid_l: close - range / 2.0,
                    bid_c: close,
                    ask_o: close + step,
                    ask_h: close + range / 2.0,
                    ask_l: close - range / 2.0,
                    ask_c: close,
                }
            })
            .collect()
    }

    /// THE POINT OF THIS WHOLE CHANGE (finding #8 of the 2026-09-13 replay↔live
    /// divergence audit).
    ///
    /// The live cron refuses a break-even amend whose target lands within
    /// `BREAKEVEN_MIN_ATR_FRACTION × ATR` of the latest close
    /// (`breakeven_decision::noise_violation` → `BreakevenBlock::InsideNoise`),
    /// and keeps the ORIGINAL stop. The replay had no such check, so the same
    /// absurd target was applied silently offline — booking a ~0R scratch on the
    /// next bar's noise where live would have run the original stop.
    ///
    /// This constructs a target that IS absurd, using a geometry the rest of the
    /// system accepts: a normal short (entry 1.1000, SL 1.1040, TP 1.0900 — R
    /// 2.5, so the min-R gate is satisfied) carrying a **mis-derived 1% arming
    /// threshold**. That arms break-even at 1.0990, one pip from entry, so the
    /// stop is moved to a price sitting essentially on top of the close that
    /// armed it. A correctly-derived break-even never looks like this — the floor
    /// is a tripwire for absurdity, not a tuning knob — which is exactly why the
    /// whole fixture corpus is expected not to move, and why this unit test
    /// rather than any fixture is the evidence that the floor works.
    ///
    /// The assertion is on the OUTCOME (which stop the position exits at), not on
    /// an internal flag: the event that *arms* is deliberately a different bar
    /// from the event that *acts* (the later bar that reaches back to a stop), so
    /// a change that merely stopped arming would not pass this by accident.
    #[test]
    fn replay_refuses_a_breakeven_that_lands_inside_the_noise_floor() {
        use trade_control_core::intent::Breakeven;

        // Short stop-entry at 1.1000, original SL 1.1040 (above), TP 1.0900 —
        // an ordinary R 2.5 setup that clears the min-R gate. The MIS-DERIVATION
        // is the threshold: 0.001 ⇒ arms at 1.1000 + 0.001×(1.0900−1.1000)
        // = 1.09990, one tick below entry. The break-even target is the fill
        // 1.1000, i.e. 0.00010 from the close that armed it.
        let mut intent = short_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.1040, 1.0900);
        intent.breakeven = Some(Breakeven { threshold: 0.001 });
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );

        let mut path = vec![fire_bar()];
        // Fill bar: reaches the 1.1000 sell-stop on the bid and closes at 1.10010
        // — ABOVE the 1.09990 arming level, so it does NOT arm.
        path.push(candle(
            "2026-06-17T11:00:00Z",
            1.1005,
            1.1006,
            1.09985,
            1.10010,
        ));
        // 30 quiet H1 bars of 0.0020 range (⇒ ATR 0.0020 ⇒ floor 0.00020) closing
        // at 1.10010, still above the arming level. They exist to WARM the ATR:
        // the floor fails open on an unwarmed window (24 bars on H1), and an arm
        // landing before warmup would make this test prove nothing — which is
        // exactly the trap the first draft of it fell into.
        path.extend(walk_down("2026-06-17T12:00:00Z", 30, 1.10010, 0.0, 0.0020));
        // NOW the arming bar: closes at 1.09990, the arming level, targeting the
        // fill 1.1000 — 0.00010 away, inside the 0.00020 floor.
        path.push(candle(
            "2026-06-18T18:00:00Z",
            1.10010,
            1.10015,
            1.09985,
            1.09990,
        ));
        // Finally a bar that runs UP through 1.1040, the ORIGINAL stop — passing
        // THROUGH the (refused) break-even level 1.1000 on the way, so the two
        // candidate stops give different exit prices.
        path.push(candle(
            "2026-06-18T19:00:00Z",
            1.0999,
            1.1045,
            1.0998,
            1.1042,
        ));

        let outcome = simulate_fill(&intent, &shell, 0.0001, &path);
        match outcome {
            SimOutcome::StoppedOut { exit_price, .. } => assert!(
                (exit_price - 1.1040).abs() < 1e-9,
                "the break-even target 1.1000 sits inside the noise floor, so the ORIGINAL stop \
                 1.1040 must still be in force; exited at {exit_price} instead — the floor was \
                 not applied"
            ),
            other => panic!("expected a stop-out at the original SL, got {other:?}"),
        }
    }

    /// The mirror of the test above, and the guard against "fix" it by simply
    /// never arming: with the SAME shape but a TP far enough away that the
    /// break-even target clears the floor, the stop DOES move and the position
    /// scratches at entry. If both tests can't hold at once, the floor is either
    /// absent (first fails) or swallowing correct break-evens (this one fails).
    #[test]
    fn replay_still_arms_a_breakeven_that_clears_the_noise_floor() {
        use trade_control_core::intent::Breakeven;

        // Same short, but TP 1.0900 — the 50% level is 1.0950 and the break-even
        // target 1.1000 is 0.0050 from the arming close: 25× the 0.00020 floor.
        let mut intent = short_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.1040, 1.0900);
        intent.breakeven = Some(Breakeven::at_half());
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );

        let mut path = vec![fire_bar()];
        // Fill bar reaches the 1.1000 sell-stop.
        path.push(candle(
            "2026-06-17T11:00:00Z",
            1.1005,
            1.1006,
            1.09990,
            1.09995,
        ));
        // Runs past the 1.0950 level on a close, arming break-even at 1.1000.
        path.push(candle(
            "2026-06-17T12:00:00Z",
            1.0990,
            1.0992,
            1.0945,
            1.0950,
        ));
        // 40 quiet bars — the SAME 0.0020 range (⇒ ATR 0.0020 ⇒ floor 0.00020) as
        // the refusing test, so the only thing that differs between the two is the
        // break-even DISTANCE: 0.0050 here versus 0.00010 there.
        path.extend(walk_down("2026-06-17T13:00:00Z", 40, 1.09500, 0.0, 0.0020));
        // A bar that runs back up THROUGH the moved stop (1.1000) and on to the
        // original (1.1040): whichever stop is in force decides the exit price.
        path.push(candle(
            "2026-06-19T10:00:00Z",
            1.0950,
            1.1045,
            1.0949,
            1.1042,
        ));

        let outcome = simulate_fill(&intent, &shell, 0.0001, &path);
        match outcome {
            SimOutcome::StoppedOut {
                exit_price,
                entry_price,
                ..
            } => {
                assert!(
                    (exit_price - 1.1000).abs() < 1e-9,
                    "this break-even clears the floor by 25×, so the stop must have moved to the \
                     fill 1.1000; exited at {exit_price} — the floor is suppressing a CORRECT \
                     break-even"
                );
                assert!((exit_price - entry_price).abs() < 1e-9, "a 0R scratch");
            }
            other => panic!("expected a break-even stop-out at entry, got {other:?}"),
        }
    }

    /// FAIL-OPEN, at the replay entry point. Same absurd geometry as
    /// `replay_refuses_a_breakeven_that_lands_inside_the_noise_floor`, but with a
    /// post-fill path too short to warm the ATR. Unjudgeable ATR means NO
    /// opinion, so the break-even arms as it always did and the position
    /// scratches at entry.
    ///
    /// This is the test a "tighten it up by failing closed" change breaks, and it
    /// is deliberately asserted at the entry point rather than on the pure
    /// predicate: a fail-closed floor at the pure layer would be caught here as a
    /// changed EXIT PRICE, which is what the corpus scores.
    #[test]
    fn replay_breakeven_fails_open_when_the_atr_is_unjudgeable() {
        use trade_control_core::intent::Breakeven;

        let mut intent = short_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.1040, 1.0900);
        intent.breakeven = Some(Breakeven { threshold: 0.001 });
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );

        // Only two post-fill bars — far short of the 24-bar H1 ATR length, so the
        // ATR is `None` and the floor has no opinion.
        let path = [
            fire_bar(),
            // Fill + arm at 1.09990, target the fill 1.1000 — the SAME absurd
            // 0.00010 distance as the test above, which there is refused.
            candle("2026-06-17T11:00:00Z", 1.1005, 1.1006, 1.09985, 1.09990),
            // Runs up through the moved stop 1.1000 and on to 1.1040.
            candle("2026-06-17T12:00:00Z", 1.09990, 1.10450, 1.09980, 1.10420),
        ];

        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut { exit_price, .. } => assert!(
                (exit_price - 1.1000).abs() < 1e-9,
                "an unwarmed ATR is unjudgeable, so the floor must FAIL OPEN and the break-even \
                 stop at 1.1000 stands; exited at {exit_price} — the floor fabricated a block out \
                 of a window it could not judge"
            ),
            other => panic!("expected a break-even stop-out at entry, got {other:?}"),
        }
    }

    /// `breakeven_armed_at` reports the **bar whose close arms break-even** —
    /// the replay stand-in for the live cron amend. Same trade-075 leg-2 geometry
    /// as `breakeven_scratches_a_leg_that_runs_50pct_then_reverses`: the
    /// `runs_past_50` bar (close 1.0940 past the 1.0950 BE level) is the arming
    /// bar, and it must match the bar that the fill sim moves the stop on.
    #[test]
    fn breakeven_armed_at_reports_the_arming_bar() {
        use trade_control_core::intent::Breakeven;
        let mut intent = short_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.1040, 1.0900); // BE level 1.0950
        intent.breakeven = Some(Breakeven::at_half());
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1005, 1.1005, 1.0995, 1.1000);
        let runs_past_50 = candle("2026-06-17T12:00:00Z", 1.0990, 1.0992, 1.0935, 1.0940);
        let bounce_to_orig_sl = candle("2026-06-17T13:00:00Z", 1.0945, 1.1041, 1.0944, 1.1000);
        let path = [fire_bar(), fill_bar, runs_past_50, bounce_to_orig_sl];

        let armed = breakeven_armed_at(&intent, &shell, 0.0001, &path, None);
        assert_eq!(
            armed,
            Some(runs_past_50.time),
            "BE arms on the bar whose close (1.0940) runs past the 1.0950 level"
        );
    }

    /// No `breakeven` rule → never armed (the field is `None`).
    #[test]
    fn breakeven_armed_at_is_none_without_a_rule() {
        let mut intent = short_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.1040, 1.0900);
        intent.breakeven = None;
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1005, 1.1005, 1.0995, 1.1000);
        let runs_past_50 = candle("2026-06-17T12:00:00Z", 1.0990, 1.0992, 1.0935, 1.0940);
        let path = [fire_bar(), fill_bar, runs_past_50];
        assert_eq!(
            breakeven_armed_at(&intent, &shell, 0.0001, &path, None),
            None
        );
    }

    /// A position stopped out at the original SL **before** any candle arms BE
    /// reports `None` — break-even never armed during its life.
    #[test]
    fn breakeven_armed_at_is_none_when_stopped_before_arming() {
        use trade_control_core::intent::Breakeven;
        let mut intent = short_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.1040, 1.0900); // BE level 1.0950
        intent.breakeven = Some(Breakeven::at_half());
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );
        // Fill, then a bar that hits the original 1.1040 SL before ever closing
        // past the 1.0950 BE level → BE never arms.
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1005, 1.1005, 1.0995, 1.1000);
        let straight_to_sl = candle("2026-06-17T12:00:00Z", 1.1005, 1.1041, 1.1000, 1.1030);
        let path = [fire_bar(), fill_bar, straight_to_sl];
        assert_eq!(
            breakeven_armed_at(&intent, &shell, 0.0001, &path, None),
            None
        );
    }

    /// A candle that only WICKS past the 50% level (but closes back short of it)
    /// must NOT arm break-even — the arming basis is the close, not the wick.
    #[test]
    fn breakeven_does_not_arm_on_a_wick() {
        use trade_control_core::intent::Breakeven;
        let mut intent = short_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.1040, 1.0900); // BE level 1.0950
        intent.breakeven = Some(Breakeven::at_half());
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );

        // Fill, then a candle whose LOW (1.0935) wicks past the 1.0950 BE level
        // but whose CLOSE (1.0960) stays short of it → BE must NOT arm. The
        // later bounce to the original SL 1.1040 then takes the full −1R.
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1005, 1.1005, 1.0995, 1.1000);
        let wick_only = candle("2026-06-17T12:00:00Z", 1.0990, 1.0992, 1.0935, 1.0960); // close 1.0960 > level
        let bounce = candle("2026-06-17T13:00:00Z", 1.0965, 1.1041, 1.0960, 1.1000);
        let path = [fire_bar(), fill_bar, wick_only, bounce];

        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!(
                    (exit_price - 1.1040).abs() < 1e-9,
                    "a wick must not arm BE; stop stays at original 1.1040, got {exit_price}"
                );
            }
            other => panic!("expected StoppedOut at original SL (no BE arm), got {other:?}"),
        }
    }

    /// The System-2 baseline (`original_stop`) is the stop the position actually
    /// carried — i.e. the signed SL **after** the System-1 entry spread floor —
    /// not the raw signed SL. This is the display-reconciliation fix: the widen
    /// must move from the *placed* stop, so the journal's order/widen/restore
    /// lines all key off one number (EUR/AUD `hs-eur-aud-3d0b5dda` showed three
    /// different SLs because System 2 used the un-floored signed level).
    #[test]
    fn widened_stop_at_baseline_is_the_floored_stop_not_the_signed_sl() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        // Long entry 1.1000, SIGNED SL 1.0995 (5p — inside the floor).
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0995, 1.1100);
        let shell = trigger_shell();
        // Fire bar carries a 3p spread (bid_c 1.10120, ask_c 1.10150) away from
        // the 1.1000 entry trigger so it doesn't fill on bar 0. 10× 3p = 30p →
        // the SL floors DOWN from 1.0995 to 1.0970.
        let fire = ba_candle("2026-06-17T10:30:00Z", 1.10150, 1.10120, 1.10150, 1.10120);
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // NY-close-edge wide bar → widen from the FLOORED baseline (1.0970).
        let wide = ba_candle("2026-06-17T21:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let path = [fire, fill_bar, wide];

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS, None)
            .expect("the NY-close-edge bar must trip the widen");
        assert!(
            (widen.original_stop - 1.0970).abs() < 1e-9,
            "baseline must be the floored stop 1.0970 (10× 3p), not the signed 1.0995; got {}",
            widen.original_stop
        );
        // And the widen moves further DOWN from the floored baseline.
        assert!(
            widen.widened_stop < 1.0970,
            "a long widen moves the SL further DOWN from the floored baseline"
        );
    }

    /// A long position whose post-fill path includes a wide-spread bar **on the
    /// NY-close edge** reports the widen: the bar's time, the original SL, and a
    /// stop moved DOWN (away from price for a long) by the clamped live spread.
    #[test]
    fn widened_stop_at_reports_the_widen_bar_for_a_long() {
        use trade_control_core::blackout_widen::{WIDEN_FLOOR_PIPS, widened_stop};
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        // Fire bar (skipped), then a zero-spread bar whose ASK reaches the 1.1000
        // long trigger → fill. Then a wide-spread bar (ask_c − bid_c = 1.10315 −
        // 1.10010 = 0.00305 = 30.5 pips, within the 22–40 clamp) that does NOT hit
        // SL/TP → widen. The wide bar sits at 21:00 UTC — the NY-close edge under
        // EDT (2026-06-17 is inside the DST window) — which the widen gate
        // requires, mirroring the live cron.
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        let wide = ba_candle("2026-06-17T21:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let spread_pips = (wide.ask_c - wide.bid_c) / 0.0001; // 30.5
        let path = [fire_bar(), fill_bar, wide];

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS, None)
            .expect("a 30.5-pip spread bar must trip the widen");
        assert_eq!(widen.at, wide.time);
        assert!((widen.original_stop - 1.0950).abs() < 1e-9);
        // 30.5 pips is within the 22–40 clamp, so widen by the live spread; long
        // moves DOWN.
        let expected = widened_stop(Direction::Long, 1.0950, spread_pips, 0.0001);
        assert!(
            (widen.widened_stop - expected).abs() < 1e-9,
            "expected {expected}, got {}",
            widen.widened_stop
        );
        assert!(
            widen.widened_stop < 1.0950,
            "a long widen moves the SL DOWN"
        );
    }

    /// A wide-spread bar that is **not** on the NY-close edge does NOT widen —
    /// the live System-2 cron only widens at the NY close (`is_ny_close_edge`),
    /// so the replay mirror must too. Without the gate this bar (12:00 UTC, a
    /// 30.5-pip spread) would trip the widen; with it, it's ignored. This is the
    /// parity gap from the EUR/AUD `hs-eur-aud-3d0b5dda` journal bug: the report
    /// mistook a NY-close-edge widen for a "wrong bar", but the real defect was
    /// the replay widening on *any* wide bar, not only the NY-close one.
    #[test]
    fn widened_stop_at_ignores_a_wide_bar_off_the_ny_close_edge() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // 30.5-pip spread — well over the floor — but 12:00 UTC is not the NY
        // close (21:00 UTC under EDT). Gate rejects it.
        let wide = ba_candle("2026-06-17T12:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let path = [fire_bar(), fill_bar, wide];
        assert_eq!(
            widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS, None),
            None,
            "a wide bar off the NY-close edge must not widen"
        );
    }

    /// Two wide-spread bars — one off the edge (12:00 UTC), one on it (21:00
    /// UTC). The widen must land on the NY-close bar, not the earlier off-edge
    /// one, proving the loop skips non-edge bars rather than firing on the first
    /// wide bar it sees.
    #[test]
    fn widened_stop_at_widens_on_the_ny_close_bar_not_the_first_wide_bar() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // Off-edge wide bar (12:00 UTC) — must be skipped.
        let off_edge = ba_candle("2026-06-17T12:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        // NY-close-edge wide bar (21:00 UTC under EDT) — must be the widen bar.
        let on_edge = ba_candle("2026-06-17T21:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let path = [fire_bar(), fill_bar, off_edge, on_edge];
        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS, None)
            .expect("the NY-close-edge bar must trip the widen");
        assert_eq!(
            widen.at, on_edge.time,
            "widen must land on the NY-close bar, not the first wide bar"
        );
    }

    /// A **baked** spread-hour instrument (TN-named `EUR/USD`, whose baked mask
    /// has bit 21 set with a ~5p p90) widens by the baked p90 via
    /// `spread_hour_widen_size`, NOT the legacy 22p `clamp_widen` floor — this
    /// is the whole point of the per-instrument path. The bar's live spread is
    /// tight (2p), so the legacy path would have needed the trigger *and* would
    /// have floored to 22p; the baked path fires on the mask alone and widens by
    /// ~5p. Depends on the committed sampler baseline (EUR/USD 21:00 spike).
    #[test]
    fn widened_stop_at_uses_baked_p90_for_a_sampled_instrument() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        use trade_control_core::spread_blackout::{spread_hour_widen_frac, widen_frac_to_pips};

        let mut intent = long_stop_intent();
        intent.instrument = "EUR/USD".into(); // TN name → in the candle table
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();

        // Guard: the candle table must actually carry EUR/USD's 21:00 spread
        // hour, else this test is vacuous. 21:00 UTC on an EDT date is a spread
        // hour. The production path (widened_stop_at) converts the widen
        // FRACTION → pips at the ORIGINAL STOP (the same reference the live cron
        // uses), so mirror that here.
        let at21 = ts("2026-06-17T21:00:00Z");
        let frac = spread_hour_widen_frac("EUR/USD", at21)
            .expect("EUR/USD must have a baked 21:00 spread-hour widen fraction");

        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // 21:00 UTC bar with a TIGHT 2p spread — the baked mask fires anyway.
        let tight = ba_candle("2026-06-17T21:00:00Z", 1.10010, 1.10005, 1.10030, 1.10025);
        let path = [fire_bar(), fill_bar, tight];

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS, None)
            .expect("the baked spread hour must trip the widen even on a tight-spread bar");
        assert_eq!(widen.at, tight.time);
        // The production widen converts the fraction at the ORIGINAL (floored)
        // stop — the same reference `widened_stop_at` and the live cron use.
        let baked = widen_frac_to_pips(frac, widen.original_stop, 0.0001);
        // Widen distance = baked p90 (~5p), NOT the 22p legacy floor. Long ⇒ SL
        // moves DOWN from the original stop by baked p90 pips.
        let expected = widen.original_stop - baked * 0.0001;
        assert!(
            (widen.widened_stop - expected).abs() < 1e-9,
            "widened by baked p90 {baked}p (expected SL {expected}), got {}",
            widen.widened_stop,
        );
        // Sanity: the baked p90 is much smaller than the legacy 22p floor, so
        // this genuinely exercises the new path.
        assert!(
            baked < WIDEN_FLOOR_PIPS,
            "baked p90 {baked} should be < 22p floor"
        );
    }

    /// Regression for `BUG-spread-hour-widen-no-subhour-lead.md`: on an **H1**
    /// stream the widen must pre-arm at the sub-candle 30-min lead instant BEFORE
    /// the flagged spread hour, not collide with the spike on the same bar and not
    /// snap to a bar boundary. GBP/AUD's baked candle mask is a single hour [21];
    /// the 21:00Z spike wicks the stop out. The replay only evaluates at bar
    /// closes, but the live 15-min cron widens mid-bar at ~20:30Z, so
    /// `spread_hour_widen_instant` reports the widen at 20:30Z (= 06:30 Brisbane) —
    /// the exact moment the live worker fires — even though this bar's open is
    /// 20:00Z.
    #[test]
    fn widened_stop_at_reports_the_sub_candle_lead_instant_on_h1() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;

        let mut intent = long_stop_intent();
        intent.instrument = "GBP/AUD".into(); // TN name, candle mask [21] only
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();

        // H1 spacing (3600s bar): fire → fill → clean 20:00 bar → 21:00 spike.
        let fill_bar = candle("2026-07-13T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // 20:00Z: the bar that LEADS INTO the flagged 21:00 hour. Tight 2p spread —
        // the legacy path would never widen here; only the mask lead does. Must not
        // hit SL(1.0950)/TP(1.1100).
        let pre_hour = ba_candle("2026-07-13T20:00:00Z", 1.10010, 1.10005, 1.10030, 1.10025);
        // 21:00Z: the spread-hour blowout (30.5p spread). If the widen only armed
        // here it would already be racing the spike.
        let spike = ba_candle("2026-07-13T21:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let path = [fire_bar(), fill_bar, pre_hour, spike];

        // The path is a clean H1 grid apart from the fire/fill warmup, so the
        // inferred bar length is 3600s → 30-min lead lands at 20:30Z inside the
        // 20:00–21:00 bar.
        assert_eq!(bar_seconds_of(&path), 3600, "path must be H1-spaced");

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS, None)
            .expect("the widen must pre-arm ahead of the 21:00 spread hour");
        assert_eq!(
            widen.at,
            ts("2026-07-13T20:30:00Z"),
            "widen must report the sub-candle 30-min lead instant (20:30Z = 06:30 Bris), \
             not the 20:00 bar open and not the 21:00 spike"
        );
    }

    /// THE MONEY-PATH FIX: `simulate_fill` scores the exit against the WIDENED
    /// stop during the spread hour, so a spike that clips the original stop but
    /// NOT the widened one no longer books a false stop-out. This is the GBP/AUD
    /// shape the operator hit: entry filled, the 21:00Z spread-hour spike wicks
    /// past the original SL, but live the broker stop was already widened clear —
    /// so the position survives. Before this fix `simulate_fill` was blind to the
    /// widen and returned StoppedOut at the original stop (a −1R the live worker
    /// never took → replay↔live divergence).
    #[test]
    fn simulate_fill_survives_spread_hour_spike_via_the_widened_stop() {
        let mut intent = short_stop_intent();
        intent.instrument = "GBP/AUD".into(); // TN mask [21]; 21:00Z is a spread hour
        // Short: sell-stop entry 1.1000, SL 1.1030 (30p above), TP 1.0950.
        // GBP/AUD's baked 21:00 widen frac (~0.00128) converts to ~14p at ~1.10,
        // so the widened SL sits ~1.1044 — above the original 1.1030.
        let shell = Shell::from_candle(
            &candle("2026-07-13T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );
        // Fill bar (short fills on the BID reaching 1.1000).
        let fill_bar = ba_candle("2026-07-13T19:00:00Z", 1.1000, 1.0999, 1.1002, 1.1001);
        // 20:00Z lead bar: tight, no exit.
        let lead = ba_candle("2026-07-13T20:00:00Z", 1.10010, 1.10005, 1.10030, 1.10025);
        // 21:00Z spike: ask HIGH 1.1035 — past the ORIGINAL SL 1.1030 (a short
        // exits on the ask) but BELOW the ~1.1044 widened SL. With the widen
        // applied the stop is NOT hit; without it, StoppedOut at 1.1030.
        let spike = ba_candle("2026-07-13T21:00:00Z", 1.10330, 1.10300, 1.10350, 1.10320);
        // 22:00Z recovered + heads to TP (bid low reaches 1.0950).
        let tp_bar = ba_candle("2026-07-13T22:00:00Z", 1.09500, 1.09480, 1.09520, 1.09500);
        let path = [fire_bar(), fill_bar, lead, spike, tp_bar];

        // Guard the premise: the reconstruction widens ABOVE the spike's ask high.
        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, 22.0, None)
            .expect("21:00Z spread hour must widen the short stop");
        assert!(
            widen.widened_stop > 1.10350,
            "widened SL {} must clear the spike ask-high 1.1035",
            widen.widened_stop
        );
        assert!(
            widen.original_stop <= 1.10300,
            "original SL {} is the one the spike wicks past",
            widen.original_stop
        );

        // The exit sim must NOT stop out on the spike — it survives to TP.
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::TookProfit { .. } => {}
            other => panic!("widen must save the trade → TookProfit, got {other:?}"),
        }
    }

    /// The widen only PROTECTS — it never invents a tighter exit. If the spike
    /// blows past even the widened stop, the position still stops out, but at the
    /// WIDENED level (the stop the live broker actually held), not the original.
    #[test]
    fn simulate_fill_stops_out_at_the_widened_stop_when_the_spike_overruns_it() {
        let mut intent = short_stop_intent();
        intent.instrument = "GBP/AUD".into();
        let shell = Shell::from_candle(
            &candle("2026-07-13T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );
        let fill_bar = ba_candle("2026-07-13T19:00:00Z", 1.1000, 1.0999, 1.1002, 1.1001);
        let lead = ba_candle("2026-07-13T20:00:00Z", 1.10010, 1.10005, 1.10030, 1.10025);
        // A monster spike: ask HIGH 1.1060 — past even the ~1.1044 widened SL.
        let spike = ba_candle("2026-07-13T21:00:00Z", 1.10560, 1.10500, 1.10600, 1.10520);
        let path = [fire_bar(), fill_bar, lead, spike];

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, 22.0, None)
            .expect("spread hour widens the stop");

        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!(
                    (exit_price - widen.widened_stop).abs() < 1e-9,
                    "must stop out at the WIDENED stop {}, not the original {}; got {exit_price}",
                    widen.widened_stop,
                    widen.original_stop,
                );
            }
            other => panic!("an overrun spike still stops out, got {other:?}"),
        }
    }

    /// The widen is **transient**: once a post-widen bar's spread recovers to/
    /// under the 4-pip cutoff, `restored_at` reports that bar — mirroring the
    /// live recovery watcher (`blackout_watch`). This is what lets the replay
    /// journal show the stop snapping back instead of a permanent widen (the
    /// EUR/AUD `hs-eur-aud-3d0b5dda` "permanent widen" journal question).
    #[test]
    fn widened_stop_at_reports_restore_on_spread_recovery() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // Widen bar: 30.5p spread on the NY-close edge (21:00 UTC, EDT).
        let wide = ba_candle("2026-06-17T21:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        // Next bar: spread back to 2p (≤ the 4p recovered cutoff) → restore here.
        let recovered = ba_candle("2026-06-17T22:00:00Z", 1.10010, 1.10005, 1.10030, 1.10025);
        let path = [fire_bar(), fill_bar, wide, recovered];

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS, None)
            .expect("the NY-close-edge bar must trip the widen");
        assert_eq!(widen.at, wide.time);
        assert_eq!(
            widen.restored_at,
            Some(recovered.time),
            "the widen must restore at the first recovered-spread bar"
        );
    }

    /// If the spread never recovers before the path ends, `restored_at` is
    /// `None` (the widen would still be active at the window's end — the 3-hour
    /// backstop hasn't landed within these bars either).
    #[test]
    fn widened_stop_at_restore_is_none_when_spread_stays_wide() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        let wide = ba_candle("2026-06-17T21:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        // Still ~30p an hour later — under the 3h backstop, spread not recovered.
        let still_wide = ba_candle("2026-06-17T22:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let path = [fire_bar(), fill_bar, wide, still_wide];

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS, None)
            .expect("the NY-close-edge bar must trip the widen");
        assert_eq!(
            widen.restored_at, None,
            "no recovery, no backstop → no restore"
        );
    }

    /// The SAFETY force-restore ceiling restores even if the spread stays
    /// elevated — the live watcher's last-resort rule. After the 2026-07 backstop
    /// split it is 12h (`SAFETY_FORCE_RESTORE_SECONDS`), not 3h — long enough to
    /// never fire mid-block. A bar ≥ 12h after the widen restores regardless of
    /// its (still-wide) spread.
    #[test]
    fn widened_stop_at_restore_fires_on_the_safety_ceiling() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        let wide = ba_candle("2026-06-17T21:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        // Still wide 3h later (the OLD backstop mark — must NOT restore now), then
        // a bar at +12h → the safety ceiling fires.
        let hour3 = ba_candle("2026-06-18T00:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let safety = ba_candle("2026-06-18T09:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let path = [fire_bar(), fill_bar, wide, hour3, safety];

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS, None)
            .expect("the NY-close-edge bar must trip the widen");
        assert_eq!(
            widen.restored_at,
            Some(safety.time),
            "the 12h safety ceiling must restore even with the spread still wide; \
             the old 3h bar must NOT have restored"
        );
    }

    /// No bar's spread reaches the trigger → no widen.
    #[test]
    fn widened_stop_at_is_none_when_spread_stays_tight() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // 2-pip spread bar — well under the 22-pip floor.
        let tight = ba_candle("2026-06-17T12:00:00Z", 1.10010, 1.10005, 1.10030, 1.10025);
        let path = [fire_bar(), fill_bar, tight];
        assert_eq!(
            widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS, None),
            None
        );
    }

    /// A position stopped out before any wide-spread bar reports no widen — the
    /// original stop was still the live one when the position closed.
    #[test]
    fn widened_stop_at_is_none_when_stopped_before_widen() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // Hits the 1.0950 SL on the bid book (long exits on bid) before any
        // wide-spread bar.
        let to_sl = candle("2026-06-17T12:00:00Z", 1.0990, 1.0991, 1.0949, 1.0951);
        let wide = ba_candle("2026-06-17T13:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let path = [fire_bar(), fill_bar, to_sl, wide];
        assert_eq!(
            widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS, None),
            None
        );
    }

    #[test]
    fn ambiguous_candle_resolves_to_stop() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();
        // One candle fills AND spans both SL and TP → pessimistic: StoppedOut.
        let both = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills @1.1050
            candle("2026-06-17T12:00:00Z", 1.1050, 1.1160, 1.0995, 1.1100), // spans SL & TP
        ];
        assert!(matches!(
            simulate_fill(&intent, &shell, 0.0001, &both),
            SimOutcome::StoppedOut { .. }
        ));
    }

    /// A short stop-entry: absolute trigger 1.1000 (below the shell close so the
    /// sell-stop is validly placed), SL 1.1030 (above), TP 1.0950 (below). Entry
    /// fills on the **bid** book (a sell); the SL closes by *buying*, so it fills
    /// on the **ask** book. The spread-specific test exploits that asymmetry: a
    /// bar whose *mid* high misses the SL but whose *ask* high reaches it stops
    /// out, because the broker closes the short on the ask.
    fn short_stop_intent() -> Intent {
        let mut i = base_enter();
        i.direction = Some(Direction::Short);
        // Absolute trigger — skips the wrong-side guard, so the test controls the
        // exact sell-stop level independent of the shell close.
        i.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 0.0,
            offset_atr_pct: None,
            at: Some(1.1000),
            recover_entry: None,
        });
        i.stop_loss = Some(PriceRef::Absolute { absolute: 1.1030 });
        i.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.0950,
        }));
        i
    }

    #[test]
    fn short_sl_triggers_on_the_ask_book() {
        let intent = short_stop_intent();
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );

        // Fill bar: the short fills on the BID — bid range must reach the 1.1000
        // trigger. Then the SL bar's MID high (1.1029) is a pip short of the
        // 1.1030 SL, but its ASK high (1.1031) reaches it. Since a short closes by
        // buying on the ask, the SL fires — a stop the mid-only view would miss.
        let sl_bar = ba_candle(
            "2026-06-17T10:30:00Z",
            1.1027, // bid_h (mid-ish high; would miss SL on the bid book)
            1.1008, // bid_l
            1.1031, // ask_h — reaches the 1.1030 SL
            1.1012, // ask_l
        );
        let fill_bar = ba_candle(
            "2026-06-17T10:15:00Z",
            1.1000, // bid_h reaches the 1.1000 sell-stop trigger
            1.0996, // bid_l
            1.1004, // ask_h
            1.1000, // ask_l
        );
        let path = [fire_bar(), fill_bar, sl_bar];

        // The ask high (1.1031) reaches the 1.1030 SL → stopped out on the ask.
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut {
                exit_price,
                entry_price,
                ..
            } => {
                // Recorded exit is the placed SL level (the resting order's price).
                assert!((exit_price - 1.1030).abs() < 1e-9);
                // Recorded entry is the placed sell-stop level (1.1000).
                assert!((entry_price - 1.1000).abs() < 1e-9);
            }
            other => panic!("expected ask-side stop-out, got {other:?}"),
        }

        // Control: if the SL bar's ASK high stops a pip *short* of the SL
        // (ask_h 1.1029 < 1.1030), the short stays open — the mid/bid reaching
        // 1.1029 must NOT trigger a close that only the ask can make.
        let near_miss = ba_candle("2026-06-17T10:45:00Z", 1.1027, 1.1008, 1.1029, 1.1012);
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &[fire_bar(), fill_bar, near_miss]),
                SimOutcome::FilledOpen { .. }
            ),
            "ask high 1.1029 misses the 1.1030 SL → still open"
        );
    }

    /// Replay/worker parity: `simulate_fill` resolves the entry via the same
    /// pure `Resolved::from_intent` the worker uses, so an **ATR-pct-buffered**
    /// enter resolves to the identical trigger on both paths. Here a short
    /// stop-entry anchored to `signal_low` with `offset_atr_pct` fills at the
    /// ATR-buffered level, proving the simulator honours the new buffer (no
    /// replay-vs-worker drift — the whole reason resolution lives in core).
    #[test]
    fn atr_buffered_short_stop_fills_at_buffered_trigger() {
        let atr = 0.0040;
        let pct = 0.5;
        // signal_low 1.1000; buffer = 0.5/100 * 0.0040 = 0.00002; short entry
        // anchors to signal_low and pushes DOWN → trigger 1.1000 - 0.00002.
        let buffered_trigger = 1.1000 - (pct / 100.0) * atr;

        let mut intent = short_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::SignalLow,
            offset_pips: 0.0,
            offset_atr_pct: Some(pct),
            at: None,
            recover_entry: None,
        });
        // Keep SL/TP absolute so the geometry is self-contained around the
        // buffered trigger (SL above, TP below — short).
        intent.stop_loss = Some(PriceRef::Absolute { absolute: 1.1030 });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.0950,
        }));
        // Real EUR_USD tick (0.00001, one finer than the 0.0001 pip) so the
        // sub-pip ATR-buffered trigger stays on-grid and isn't snapped away by
        // the resolver's rounding — mirrors the baked `Intent::tick_size` a real
        // armed EUR_USD trade carries.
        intent.tick_size = Some(0.00001);

        // Shell carries the latched pattern low + ATR; close sits between the
        // trigger and SL so the short stop is correct-side (trigger < close).
        let mut shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1015, 1.1018, 1.1010, 1.1012).mid(),
        );
        shell.signal_low = Some(1.1000);
        shell.signal_high = Some(1.1030);
        shell.atr = Some(atr);

        // A bar whose bid reaches the buffered sell-stop trigger fills the short.
        let fill_bar = ba_candle(
            "2026-06-17T10:15:00Z",
            buffered_trigger, // bid_h reaches the buffered trigger
            buffered_trigger - 0.0010,
            buffered_trigger + 0.0004,
            buffered_trigger,
        );
        match simulate_fill(&intent, &shell, 0.0001, &[fire_bar(), fill_bar]) {
            SimOutcome::FilledOpen { entry_price, .. } => {
                assert!(
                    (entry_price - buffered_trigger).abs() < 1e-9,
                    "filled at {entry_price}, expected ATR-buffered {buffered_trigger}"
                );
            }
            other => panic!("expected fill at the ATR-buffered trigger, got {other:?}"),
        }
    }

    /// The fail-closed half: an ATR-pct enter whose shell carries **no ATR**
    /// (warmup) is `Unresolved` in the simulator too — same reject the worker
    /// gives — rather than silently filling at a zero-buffer level.
    #[test]
    fn atr_buffered_enter_with_no_atr_is_unresolved() {
        let mut intent = short_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::SignalLow,
            offset_pips: 0.0,
            offset_atr_pct: Some(0.5),
            at: None,
            recover_entry: None,
        });
        let mut shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1015, 1.1018, 1.1010, 1.1012).mid(),
        );
        shell.signal_low = Some(1.1000);
        shell.atr = None; // warmup / short feed
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &[fire_bar()]),
                SimOutcome::Unresolved(_)
            ),
            "no-ATR ATR-pct enter must be Unresolved, not a zero-buffer fill"
        );
    }

    #[test]
    fn pending_entry_does_not_fill_on_the_fire_bar() {
        // The off-by-one regression (CAD/JPY 21-May confirmed short): the enter
        // fires on the confirming bar, but a resting Stop/Limit order isn't live
        // until that bar *closes*. So even if the fire bar's own range crosses the
        // trigger, the fill must wait for the NEXT bar. A live worker places the
        // order on the cron tick after the bar closes — it cannot fill the bar
        // whose close produced the confirmation.
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();

        // Fire bar already trades through the 1.1050 trigger. The OLD code filled
        // here (index 0); the fix skips it.
        let fire_crosses = candle("2026-06-17T10:30:00Z", 1.1045, 1.1060, 1.1044, 1.1052);

        // Case 1: ONLY the fire bar crosses; nothing after → NeverFilled (not a
        // fill on the fire bar).
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &[fire_crosses]),
            SimOutcome::NeverFilled,
            "a trigger-crossing fire bar must not fill — order not live until it closes"
        );

        // Case 2: fire bar crosses AND a later bar also reaches the trigger → the
        // fill is recorded on the LATER bar (11:00), never the fire bar (10:30).
        let path = [
            fire_crosses,
            candle("2026-06-17T11:00:00Z", 1.1048, 1.1055, 1.1047, 1.1052), // fills here
            candle("2026-06-17T12:00:00Z", 1.1052, 1.1160, 1.1050, 1.1155), // TP 1.1150
        ];
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::TookProfit { fill_at, .. } => {
                assert_eq!(
                    fill_at,
                    ts("2026-06-17T11:00:00Z"),
                    "fill must be the post-fire bar, not the fire bar"
                );
            }
            other => panic!("expected TookProfit filled on the post-fire bar, got {other:?}"),
        }
    }

    #[test]
    fn fill_bar_can_stop_out_same_bar() {
        // A pending order that fills mid-bar can be stopped out later in that
        // SAME bar (a violent breakout: spikes through the entry, then reverses
        // to the SL before the bar closes). The exit search must include the fill
        // bar; with only OHLC the pessimistic tie-break calls a fill-bar that
        // also spans the SL a stop-out.
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050; SL 1.1000, TP 1.1150
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();

        // Bar 1 (post-fire) fills @1.1050 AND its range reaches the SL 1.1000 in
        // the same bar → StoppedOut, exit on this same bar.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1048, 1.1055, 1.0995, 1.1010), // fill + SL
        ];
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut {
                fill_at, exit_at, ..
            } => {
                assert_eq!(fill_at, ts("2026-06-17T11:00:00Z"));
                assert_eq!(
                    exit_at,
                    ts("2026-06-17T11:00:00Z"),
                    "the stop-out lands on the fill bar itself"
                );
            }
            other => panic!("expected same-bar StoppedOut, got {other:?}"),
        }

        // Control: a fill bar that fills but does NOT reach SL/TP stays open.
        let still_open = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1048, 1.1055, 1.1047, 1.1052), // fill only
        ];
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &still_open),
                SimOutcome::FilledOpen { .. }
            ),
            "a fill bar that doesn't reach SL/TP must stay open, not exit"
        );
    }

    #[test]
    fn bar_expiry_cancels_a_late_fill() {
        // `expiry_bars = 2`: the order is live only for the 2 bars after the fire
        // bar. A trigger cross on the 3rd bar (or later) is an order the worker
        // would already have cancelled → NeverFilled.
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.expiry_bars = Some(Tunable::Static(2));
        let shell = trigger_shell();

        let no_cross = |t: &str| candle(t, 1.1041, 1.1045, 1.1038, 1.1043);

        // Cross only on bar 3 after the fire bar (index 3 of the path) → expired.
        let late = [
            fire_bar(),                                                     // bar 0 (fire)
            no_cross("2026-06-17T11:00:00Z"),                               // bar 1 (live)
            no_cross("2026-06-17T12:00:00Z"),                               // bar 2 (live, last)
            candle("2026-06-17T13:00:00Z", 1.1048, 1.1055, 1.1047, 1.1052), // bar 3 — too late
        ];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &late),
            SimOutcome::NeverFilled,
            "a cross after expiry_bars must not fill — order already cancelled"
        );

        // Cross on bar 2 (the last live bar) → still fills.
        let in_time = [
            fire_bar(),
            no_cross("2026-06-17T11:00:00Z"),
            candle("2026-06-17T12:00:00Z", 1.1048, 1.1055, 1.1047, 1.1052), // bar 2 — fills
        ];
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &in_time),
                SimOutcome::FilledOpen { .. }
            ),
            "a cross on the last live bar still fills"
        );
    }

    // --- sweep_reason -------------------------------------------------------

    /// A never-triggered LONG stop-entry whose price falls past its SL while the
    /// order is still resting → the live cron sweep would cancel it for an
    /// SL-breach. `sweep_reason` reports that, at the breaching bar.
    #[test]
    fn sweep_reason_reports_sl_breach() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050; SL 1.1000
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();

        // Fire bar (skipped), then two bars that never reach the 1.1050 trigger;
        // the second CLOSES at 1.0995 — below the 1.1000 SL → breach.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1040), // rests, no breach
            candle("2026-06-17T12:00:00Z", 1.1010, 1.1012, 1.0990, 1.0995), // close past SL
        ];
        // Sanity: this path is NeverFilled (trigger never reached).
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::NeverFilled
        );
        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &path),
            Some((SweepReason::SlBreached, ts("2026-06-17T12:00:00Z")))
        );
    }

    /// A never-triggered stop-entry whose bar-based `cancel_at` (off the shell's
    /// Pine forward-bar-close menu) passes → swept for bar-expiry.
    #[test]
    fn sweep_reason_reports_bar_expiry() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.expiry_bars = Some(Tunable::Static(1));
        // cancel_at resolves off slot-1 of the shell's forward menu.
        let mut shell = trigger_shell();
        shell.next_candle_timestamp_1 = Some(ts("2026-06-17T11:30:00Z"));

        // Bars never reach the trigger and never breach the SL — only bar-expiry
        // can fire. The 12:00 bar is past the 11:30 cancel_at.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043), // before cancel_at
            candle("2026-06-17T12:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043), // past cancel_at
        ];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::NeverFilled
        );
        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &path),
            Some((SweepReason::BarExpiry, ts("2026-06-17T12:00:00Z")))
        );
    }

    /// A never-triggered stop-entry whose alert window (`not_after`) closes
    /// during the candle path → swept as alert-window expired. Expiry takes
    /// priority over a same-bar SL-breach (worker `sweep_one` branch order).
    #[test]
    fn sweep_reason_reports_alert_window_expiry_first() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050; SL 1.1000
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.not_after = ts("2026-06-17T11:30:00Z");
        let shell = trigger_shell();

        // The 12:00 bar is past not_after AND closes past the SL — expiry wins.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043), // within window
            candle("2026-06-17T12:00:00Z", 1.1010, 1.1012, 1.0990, 1.0995), // past window + SL
        ];
        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &path),
            Some((SweepReason::Expired, ts("2026-06-17T12:00:00Z")))
        );
    }

    /// No sweep condition reached within the path → `None`. A resting order that
    /// simply never triggered (and never breached / expired) is not swept.
    #[test]
    fn sweep_reason_is_none_when_nothing_sweeps() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050; SL 1.1000
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        // No bar-expiry, generous alert window.
        intent.expiry_bars = None;
        let shell = trigger_shell();

        // Bars stay between SL and trigger — never fill, never breach, in-window.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043),
            candle("2026-06-17T12:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043),
        ];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::NeverFilled
        );
        assert_eq!(sweep_reason(&intent, &shell, 0.0001, &path), None);
    }

    /// A Market entry never rests, so a (degenerate) Market `NeverFilled` is not
    /// a swept order → `None`.
    #[test]
    fn sweep_reason_is_none_for_market_entry() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Market);
        let shell = trigger_shell();
        assert_eq!(sweep_reason(&intent, &shell, 0.0001, &[fire_bar()]), None);
    }

    /// A never-triggered stop-entry whose resting bars fall inside a market-hours
    /// blackout window → swept as `Blackout`. Blackout takes priority over a
    /// same-bar SL-breach (worker `sweep_one` branch order: blackout before the
    /// stale-price SL check). Blackout is now read from the baked weekday-aware
    /// mask keyed on the instrument (`EUR_USD`, weekend-only). A Friday-night bar
    /// sits inside the universal weekend halt and must win over SL-breach; a
    /// mid-week bar (no daily-close for EUR_USD) falls through to SL-breach.
    #[test]
    fn sweep_reason_reports_market_blackout() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050; SL 1.1000
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.expiry_bars = None;
        assert_eq!(
            intent.instrument, "EUR_USD",
            "baked weekend-only instrument"
        );
        let shell = trigger_shell();

        // 2026-06-19 is a FRIDAY. The 22:00Z bar both closes past the SL AND
        // sits inside the weekend halt (Fri 21:00Z → Sun 22:00Z). Blackout must
        // win over SL-breach.
        let fri_path = [
            fire_bar(),
            candle("2026-06-19T20:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043), // Fri pre-halt, no breach
            candle("2026-06-19T22:00:00Z", 1.1010, 1.1012, 1.0990, 1.0995), // Fri in-halt + past SL
        ];
        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &fri_path),
            Some((SweepReason::Blackout, ts("2026-06-19T22:00:00Z")))
        );

        // 2026-06-17 is a WEDNESDAY — EUR_USD has no mid-week daily close, so
        // the same past-SL bar falls through to SL-breach, not blackout.
        let wed_path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043),
            candle("2026-06-17T12:00:00Z", 1.1010, 1.1012, 1.0990, 1.0995),
        ];
        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &wed_path),
            Some((SweepReason::SlBreached, ts("2026-06-17T12:00:00Z")))
        );
    }

    // --- pre-fill SL breach cancels the resting order (replay==live) --------

    /// The candle path that reproduces the pre-fill SL-breach divergence: a long
    /// stop-entry (trigger 1.1050, SL 1.1000) that is NEVER reached, price then
    /// falls clean through the 1.1000 stop-loss on bar 2 (close 1.0995), and on
    /// bar 3 rallies back up THROUGH the 1.1050 trigger.
    ///
    /// Live, the cron sweep cancels the order the moment bar 2's price overtakes
    /// the SL — the setup invalidated before it ever filled — so bar 3's rally can
    /// never fill it. A replay that lets bar 3 fill books a trade production
    /// structurally could not take.
    fn breach_then_rally_path() -> [BidAskCandle; 4] {
        [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1040), // rests, no breach
            candle("2026-06-17T12:00:00Z", 1.1010, 1.1012, 1.0990, 1.0995), // close past the 1.1000 SL
            candle("2026-06-17T13:00:00Z", 1.1020, 1.1060, 1.1015, 1.1055), // rallies through 1.1050
        ]
    }

    /// A long stop-entry intent whose ONLY sweep reason can be the SL breach:
    /// no bar-expiry, a generous alert window, and a mid-week (non-blackout) path.
    fn breach_intent() -> Intent {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050; SL 1.1000
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.expiry_bars = None;
        intent
    }

    /// The bug this fixes, pinned from the outside: `sweep_reason` (the shared
    /// decision) says the live cron cancels this order for an SL breach on the
    /// 12:00 bar — so the 13:00 rally through the trigger must NOT fill.
    ///
    /// Before the fix `find_fill` had no breach condition and returned
    /// `FilledOpen` here, inventing a trade the live account could never hold.
    #[test]
    fn pre_fill_sl_breach_blocks_a_later_fill() {
        let intent = breach_intent();
        let shell = trigger_shell();
        let path = breach_then_rally_path();

        // The shared sweep decision agrees the live worker cancels this order,
        // at the 12:00 bar, for an SL breach.
        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &path),
            Some((SweepReason::SlBreached, ts("2026-06-17T12:00:00Z"))),
            "the live cron sweep cancels this resting order on the 12:00 breach"
        );

        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::NeverFilled,
            "a swept order must not fill on a later rally back through the trigger"
        );
    }

    /// The mirror for a SHORT: price runs UP through the short's stop-loss while
    /// the short-stop entry below is still resting, then falls back through the
    /// trigger. `breach_detected` is direction-dependent, so both signs are pinned
    /// — a fix that only handled `Direction::Long` would pass the test above.
    #[test]
    fn pre_fill_sl_breach_blocks_a_later_fill_for_a_short() {
        let mut intent = long_stop_intent();
        intent.direction = Some(Direction::Short);
        // A short stop must sit BELOW the close (1.1040), and `resolve_offset`
        // adds the offset signed as written — so a short's trigger needs a
        // NEGATIVE offset to land under the close.
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: -10.0, // short stop → trigger 1.1030
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.stop_loss = Some(PriceRef::Absolute { absolute: 1.1080 });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.0950,
        }));
        intent.expiry_bars = None;
        let shell = trigger_shell();

        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1040), // rests, no breach
            candle("2026-06-17T12:00:00Z", 1.1070, 1.1095, 1.1068, 1.1090), // close past the 1.1080 SL
            candle("2026-06-17T13:00:00Z", 1.1060, 1.1062, 1.1020, 1.1025), // falls through 1.1030
        ];

        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &path),
            Some((SweepReason::SlBreached, ts("2026-06-17T12:00:00Z"))),
            "the live cron sweep cancels the short's resting order on the 12:00 breach"
        );
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::NeverFilled,
            "a swept short order must not fill on a later fall back through the trigger"
        );
    }

    /// WICK, not close (2026-09-14). A bar that trades clean through the stop and
    /// **closes back inside** is a breach: the rule is "has price TRADED past the
    /// stop since placement".
    ///
    /// This bar's low (1.0990) is past the 1.1000 SL while its close (1.1035) is
    /// comfortably above it. Under the previous close-sampling the sweep saw
    /// nothing, the window was never truncated, and the 13:00 rally filled an
    /// order the live worker (which polls a quote per tick, and now carries a
    /// persisted running extreme) would have cancelled. That is exactly the case
    /// the rule exists to catch, and the reason close-sampling was rejected.
    ///
    /// Both the truncation (which changes outcomes) and `sweep_reason` (which
    /// labels them) are asserted, because they are two separate call sites of the
    /// same decision and a fix to one alone leaves the journal lying about the
    /// other.
    #[test]
    fn a_bar_that_wicks_through_the_sl_and_closes_back_is_a_breach() {
        let intent = breach_intent();
        let shell = trigger_shell();
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1040),
            // Low 1.0990 pierces the 1.1000 SL; close 1.1035 is back above it.
            candle("2026-06-17T12:00:00Z", 1.1030, 1.1038, 1.0990, 1.1035),
            candle("2026-06-17T13:00:00Z", 1.1020, 1.1060, 1.1015, 1.1055), // would fill at 1.1050
        ];

        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &path),
            Some((SweepReason::SlBreached, ts("2026-06-17T12:00:00Z"))),
            "the wick through the stop is the breach — the close is irrelevant",
        );
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::NeverFilled,
            "a wick-breached order must not fill on the later rally",
        );
    }

    /// The SHORT mirror: a bar whose HIGH pierces the short's stop while its close
    /// sits back below it. Pinned separately because
    /// `bar_adverse_extreme` is direction-dependent, and a Long-hardcoded reading
    /// would take this bar's low — which for a short is the favourable side and
    /// never breaches.
    #[test]
    fn a_short_bar_that_wicks_through_the_sl_and_closes_back_is_a_breach() {
        let mut intent = long_stop_intent();
        intent.direction = Some(Direction::Short);
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: -10.0, // short stop → trigger 1.1030
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.stop_loss = Some(PriceRef::Absolute { absolute: 1.1080 });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.0950,
        }));
        intent.expiry_bars = None;
        let shell = trigger_shell();

        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1040),
            // High 1.1095 pierces the 1.1080 SL; close 1.1045 is back below it.
            candle("2026-06-17T12:00:00Z", 1.1050, 1.1095, 1.1044, 1.1045),
            candle("2026-06-17T13:00:00Z", 1.1060, 1.1062, 1.1020, 1.1025), // would fill at 1.1030
        ];

        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &path),
            Some((SweepReason::SlBreached, ts("2026-06-17T12:00:00Z"))),
            "a short breaches on the HIGH — a Long-hardcoded extreme reads the low and misses it",
        );
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::NeverFilled,
        );
    }

    /// Teeth on the other side of the wick rule: a bar that comes CLOSE to the
    /// stop without reaching it is not a breach. Without this, "any bar near the
    /// stop breaches" would pass both wick tests above while cancelling every
    /// resting order in the corpus.
    #[test]
    fn a_wick_that_stops_short_of_the_sl_is_not_a_breach() {
        let intent = breach_intent();
        let shell = trigger_shell();
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1040),
            // Low 1.1001 — one tick ABOVE the 1.1000 SL. Not a breach.
            candle("2026-06-17T12:00:00Z", 1.1030, 1.1038, 1.1001, 1.1035),
            candle("2026-06-17T13:00:00Z", 1.1020, 1.1060, 1.1015, 1.1055),
        ];
        assert_eq!(sweep_reason(&intent, &shell, 0.0001, &path), None);
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &path),
                SimOutcome::FilledOpen { .. }
            ),
            "a wick that never reaches the stop leaves the order alone",
        );
    }

    /// The teeth on the other side: an order whose SL is NEVER breached before it
    /// fills must still fill exactly as before. Without this, "cancel everything"
    /// would pass the two tests above while retiring the whole fill path.
    #[test]
    fn an_unbreached_order_still_fills_normally() {
        let intent = breach_intent();
        let shell = trigger_shell();

        // Same shape as the breach path but bar 2 dips only to 1.1005 — above the
        // 1.1000 SL — so nothing sweeps and the 13:00 rally fills.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1040),
            candle("2026-06-17T12:00:00Z", 1.1030, 1.1032, 1.1005, 1.1010), // never past SL
            candle("2026-06-17T13:00:00Z", 1.1020, 1.1060, 1.1015, 1.1055), // fills at 1.1050
        ];
        assert_eq!(sweep_reason(&intent, &shell, 0.0001, &path), None);
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &path),
                SimOutcome::FilledOpen { .. }
            ),
            "an order the sweep never touches must still fill"
        );
    }

    /// A fill that happens BEFORE the breach bar is a real trade and must be
    /// kept: the sweep only ever cancels a *resting* order, so a breach after the
    /// fill is just the position's own stop-out, not a cancel. Pins that the new
    /// condition is scoped to the pre-fill window and does not retro-cancel.
    #[test]
    fn a_breach_after_the_fill_is_a_stop_out_not_a_cancel() {
        let intent = breach_intent();
        let shell = trigger_shell();

        // Bar 1 fills at 1.1050; bar 2 then runs down through the 1.1000 SL.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1045, 1.1060, 1.1043, 1.1055), // fills
            candle("2026-06-17T12:00:00Z", 1.1010, 1.1012, 1.0990, 1.0995), // stops out
        ];
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &path),
                SimOutcome::StoppedOut { .. }
            ),
            "a breach after the fill is the position's stop-out, not a sweep"
        );
    }

    /// The SHORT mirror of `an_unbreached_order_still_fills_normally`, and the
    /// test that actually pins the breach predicate's DIRECTION.
    ///
    /// A short's ordinary resting bars sit *below* its stop-loss, so a
    /// wrong-direction predicate (`Long`'s `current <= sl`) reads every one of
    /// them as a breach, truncates the window at the first bar, and still yields
    /// `NeverFilled` — which is why the short breach test alone cannot tell a
    /// correct implementation from a direction-swapped one. Here the short MUST
    /// fill, so a swapped predicate cuts the window before the fill bar and goes
    /// red. (Mutation-verified: hardcoding `Direction::Long` in
    /// `truncate_at_pre_fill_sl_breach` survives every other test and is killed
    /// only by this one.)
    #[test]
    fn an_unbreached_short_still_fills_normally() {
        let mut intent = long_stop_intent();
        intent.direction = Some(Direction::Short);
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: -10.0, // short stop → trigger 1.1030
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.stop_loss = Some(PriceRef::Absolute { absolute: 1.1080 });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.0950,
        }));
        intent.expiry_bars = None;
        let shell = trigger_shell();

        // Bars rest between the 1.1030 trigger and the 1.1080 SL — no breach —
        // then bar 2 falls through the trigger and fills.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1040),
            candle("2026-06-17T12:00:00Z", 1.1038, 1.1040, 1.1020, 1.1025), // fills at 1.1030
        ];
        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &path),
            None,
            "nothing sweeps this short — its closes stay below the 1.1080 SL"
        );
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &path),
                SimOutcome::FilledOpen { .. }
            ),
            "an unbreached short must still fill — a Long-direction breach test \
             would read these below-SL bars as breaches and cut the window"
        );
    }

    /// The boundary: a bar that BOTH reaches the trigger and closes past the SL
    /// still fills. The live sweep acts on the cron tick that observes that bar's
    /// close — by then the order could already have filled intrabar — so the
    /// breaching bar stays in the window and only the bars AFTER it are cut.
    /// This is the same exclusive-of-the-cancel-bar convention `expiry_bars` uses.
    #[test]
    fn the_breaching_bar_itself_can_still_fill() {
        let intent = breach_intent();
        let shell = trigger_shell();

        // One bar that rallies through 1.1050 and then collapses past the 1.1000
        // SL, closing at 1.0995. It fills (and Phase 2 stops it out on that bar).
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1045, 1.1060, 1.0990, 1.0995),
        ];
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &path),
                SimOutcome::StoppedOut { .. }
            ),
            "the order was live during the breaching bar, so it fills and stops out"
        );
    }

    // --- sub-bar zoom on an ambiguous SL/TP bar (PR-2) ---------------------
    //
    // A long stop entry (trigger 1.1050, SL 1.1000, TP 1.1150). We fill on bar 1
    // and then hand bar 2 a range that straddles BOTH the SL and the TP — the
    // coarse bar can't tell which was hit first. `simulate_fill` (no zoom) keeps
    // the pessimistic stop; `simulate_fill_resolved_zoom` with a finer series
    // decides by whichever sub-bar touches a level first.

    /// A fixed set of sub-bars keyed to the ambiguous parent bar's window — the
    /// offline replay's pre-fetched finer series, but hand-built for the test.
    struct FakeSubBars(Vec<BidAskCandle>);

    impl SubBars for FakeSubBars {
        fn sub_bars(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<BidAskCandle> {
            self.0
                .iter()
                .filter(|c| c.time >= start && c.time < end)
                .cloned()
                .collect()
        }
    }

    /// A long stop intent with the offset entry (trigger 1.1050), resolved SL
    /// 1.1000 / TP 1.1150, and the fill path up to (but excluding) the ambiguous
    /// exit bar — shared by the zoom tests below.
    fn ambiguous_long_setup() -> (Intent, Shell) {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        (intent, trigger_shell())
    }

    /// The coarse exit path: fire bar, a fill bar reaching 1.1050, then the
    /// AMBIGUOUS bar whose range spans 1.0990..1.1160 (touches SL 1.1000 AND TP
    /// 1.1150). Hourly bars, so the inferred bar length is 1h and the zoom window
    /// for the ambiguous bar is [13:00, 14:00).
    fn ambiguous_exit_path() -> [BidAskCandle; 3] {
        [
            fire_bar(),
            candle("2026-06-17T12:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills @1.1050
            candle("2026-06-17T13:00:00Z", 1.1050, 1.1160, 1.0990, 1.1100), // BOTH SL & TP
        ]
    }

    #[test]
    fn ambiguous_bar_without_zoom_is_pessimistic_stop() {
        let (intent, shell) = ambiguous_long_setup();
        let path = ambiguous_exit_path();
        // No provider → the plain entry point → pessimistic stop.
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!(
                    (exit_price - 1.1000).abs() < 1e-9,
                    "SL 1.1000, got {exit_price}"
                );
            }
            other => panic!("no-zoom must pessimistically stop, got {other:?}"),
        }
    }

    #[test]
    fn zoom_picks_take_profit_when_a_sub_bar_hits_tp_first() {
        let (intent, shell) = ambiguous_long_setup();
        let path = ambiguous_exit_path();
        let resolved = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).expect("resolves");
        // Finer sub-bars inside [13:00, 14:00): the FIRST touches only the TP
        // (high 1.1155 ≥ 1.1150, low stays above the SL); a later one would hit
        // the SL, but TP already resolved the exit.
        let subs = FakeSubBars(vec![
            candle("2026-06-17T13:00:00Z", 1.1052, 1.1155, 1.1051, 1.1150), // TP first
            candle("2026-06-17T13:30:00Z", 1.1150, 1.1151, 1.0990, 1.0995), // SL later
        ]);
        match simulate_fill_resolved_zoom(&resolved, &intent, &shell, 0.0001, &path, &subs) {
            SimOutcome::TookProfit {
                exit_price,
                exit_at,
                ..
            } => {
                assert!(
                    (exit_price - 1.1150).abs() < 1e-9,
                    "TP 1.1150, got {exit_price}"
                );
                // Exit stamped at the PARENT bar's open time, not the sub-bar's.
                assert_eq!(exit_at, ts("2026-06-17T13:00:00Z"));
            }
            other => panic!("zoom must take profit (TP sub-bar first), got {other:?}"),
        }
    }

    #[test]
    fn zoom_picks_stop_when_a_sub_bar_hits_sl_first() {
        let (intent, shell) = ambiguous_long_setup();
        let path = ambiguous_exit_path();
        let resolved = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).expect("resolves");
        // Mirror: the FIRST sub-bar touches only the SL; TP only later.
        let subs = FakeSubBars(vec![
            candle("2026-06-17T13:00:00Z", 1.1050, 1.1051, 1.0990, 1.0995), // SL first
            candle("2026-06-17T13:30:00Z", 1.0995, 1.1155, 1.0994, 1.1150), // TP later
        ]);
        match simulate_fill_resolved_zoom(&resolved, &intent, &shell, 0.0001, &path, &subs) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!(
                    (exit_price - 1.1000).abs() < 1e-9,
                    "SL 1.1000, got {exit_price}"
                );
            }
            other => panic!("zoom must stop (SL sub-bar first), got {other:?}"),
        }
    }

    #[test]
    fn zoom_falls_back_to_stop_when_a_sub_bar_is_itself_ambiguous() {
        let (intent, shell) = ambiguous_long_setup();
        let path = ambiguous_exit_path();
        let resolved = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).expect("resolves");
        // The FIRST (and only) sub-bar still straddles both levels — the finest
        // grain we have can't order them, so we stay pessimistic.
        let subs = FakeSubBars(vec![candle(
            "2026-06-17T13:00:00Z",
            1.1050,
            1.1160,
            1.0990,
            1.1100,
        )]);
        match simulate_fill_resolved_zoom(&resolved, &intent, &shell, 0.0001, &path, &subs) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!((exit_price - 1.1000).abs() < 1e-9);
            }
            other => panic!("ambiguous sub-bar must stay pessimistic stop, got {other:?}"),
        }
    }

    #[test]
    fn zoom_falls_back_to_stop_when_no_sub_bars_cover_the_window() {
        let (intent, shell) = ambiguous_long_setup();
        let path = ambiguous_exit_path();
        let resolved = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).expect("resolves");
        // A provider whose sub-bars fall OUTSIDE [13:00, 14:00) → empty for this
        // window → pessimistic stop (the finer feed didn't cover the move).
        let subs = FakeSubBars(vec![candle(
            "2026-06-17T15:00:00Z",
            1.1150,
            1.1155,
            1.1149,
            1.1150,
        )]);
        match simulate_fill_resolved_zoom(&resolved, &intent, &shell, 0.0001, &path, &subs) {
            SimOutcome::StoppedOut { .. } => {}
            other => panic!("no covering sub-bars must stay pessimistic stop, got {other:?}"),
        }
    }

    // --- two-pass LAZY zoom (pass 1 records, pass 2 serves) -----------------
    //
    // The lazy zoom replaces the eager whole-window finer pull with: run the sim
    // under a recorder that serves nothing, fetch only the windows it asked for,
    // re-run with those. Its soundness rests on ONE property — pass 1 asks for
    // exactly the window pass 2 needs — which the first test below pins down.

    use super::super::lazy_zoom::{RecordingSubBars, WindowSubBars, ZoomWindow};

    #[test]
    fn pass_one_records_the_ambiguous_bars_sub_window() {
        let (intent, shell) = ambiguous_long_setup();
        let path = ambiguous_exit_path();
        let resolved = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).expect("resolves");

        let rec = RecordingSubBars::new();
        let outcome = simulate_fill_resolved_zoom(&resolved, &intent, &shell, 0.0001, &path, &rec);

        // Serving nothing ⇒ identical to `NoZoom`: the pessimistic stop.
        assert!(
            matches!(outcome, SimOutcome::StoppedOut { .. }),
            "recorder must behave like NoZoom, got {outcome:?}"
        );
        // And it recorded the ambiguous bar's window: the parent bar opens at
        // 13:00 and the inferred bar length is 1h.
        assert_eq!(
            rec.windows(),
            vec![ZoomWindow {
                start: ts("2026-06-17T13:00:00Z"),
                end: ts("2026-06-17T14:00:00Z"),
            }],
        );
    }

    /// The invariant the whole two-pass design rests on: the window pass 1 asks
    /// for does not depend on what the provider serves. If a provider could
    /// change which bars get zoomed, the narrow fetch could miss the bar pass 2
    /// then needs, and the lazy zoom would silently score differently from the
    /// eager one.
    ///
    /// Verified by running the SAME sim under providers with opposite outcomes
    /// (one resolves to TP, one to SL) and asserting both requested the same
    /// window — even though they disagree about the result.
    #[test]
    fn zoom_requests_are_invariant_to_the_provider() {
        let (intent, shell) = ambiguous_long_setup();
        let path = ambiguous_exit_path();
        let resolved = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).expect("resolves");

        /// Records like `RecordingSubBars` but also serves a fixed series, so we
        /// can vary the OUTCOME while watching the REQUESTS.
        struct RecordAndServe {
            inner: RecordingSubBars,
            serve: Vec<BidAskCandle>,
        }
        impl SubBars for RecordAndServe {
            fn sub_bars(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<BidAskCandle> {
                self.inner.sub_bars(start, end);
                self.serve
                    .iter()
                    .filter(|c| c.time >= start && c.time < end)
                    .cloned()
                    .collect()
            }
        }

        let tp_first = RecordAndServe {
            inner: RecordingSubBars::new(),
            serve: vec![candle(
                "2026-06-17T13:00:00Z",
                1.1052,
                1.1155,
                1.1051,
                1.1150,
            )],
        };
        let sl_first = RecordAndServe {
            inner: RecordingSubBars::new(),
            serve: vec![candle(
                "2026-06-17T13:00:00Z",
                1.1050,
                1.1051,
                1.0990,
                1.0995,
            )],
        };

        let a = simulate_fill_resolved_zoom(&resolved, &intent, &shell, 0.0001, &path, &tp_first);
        let b = simulate_fill_resolved_zoom(&resolved, &intent, &shell, 0.0001, &path, &sl_first);

        // The providers genuinely disagree about the outcome...
        assert!(matches!(a, SimOutcome::TookProfit { .. }), "got {a:?}");
        assert!(matches!(b, SimOutcome::StoppedOut { .. }), "got {b:?}");
        // ...yet asked for exactly the same window, and the same one the
        // serve-nothing recorder asked for.
        let expected = vec![ZoomWindow {
            start: ts("2026-06-17T13:00:00Z"),
            end: ts("2026-06-17T14:00:00Z"),
        }];
        assert_eq!(tp_first.inner.windows(), expected);
        assert_eq!(sl_first.inner.windows(), expected);
    }

    #[test]
    fn no_ambiguous_bar_records_no_windows_so_nothing_is_fetched() {
        let (intent, shell) = ambiguous_long_setup();
        // Exit bar touches ONLY the TP — unambiguous, so the sim never zooms.
        let path = [
            fire_bar(),
            candle("2026-06-17T12:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052),
            candle("2026-06-17T13:00:00Z", 1.1052, 1.1155, 1.1051, 1.1150),
        ];
        let resolved = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).expect("resolves");

        let rec = RecordingSubBars::new();
        let outcome = simulate_fill_resolved_zoom(&resolved, &intent, &shell, 0.0001, &path, &rec);

        assert!(
            matches!(outcome, SimOutcome::TookProfit { .. }),
            "{outcome:?}"
        );
        assert!(
            rec.windows().is_empty(),
            "an unambiguous exit must request NO zoom window (the whole point: \
             this replay fetches no finer candles at all), got {:?}",
            rec.windows()
        );
    }

    /// End-to-end: the two-pass lazy zoom reaches the same outcome the eager
    /// whole-window pull does, while only ever holding the recorded window's
    /// candles. This is the parity claim — lazy is a PERFORMANCE change.
    #[test]
    fn two_pass_lazy_zoom_matches_the_eager_whole_window_zoom() {
        let (intent, shell) = ambiguous_long_setup();
        let path = ambiguous_exit_path();
        let resolved = Resolved::from_intent(&intent, &shell, 0.0001, 0.0).expect("resolves");

        // The "broker": a full finer series spanning the WHOLE coarse window,
        // most of which the eager pull would fetch and never consult.
        let whole_window = vec![
            candle("2026-06-17T11:00:00Z", 1.1040, 1.1045, 1.1039, 1.1042),
            candle("2026-06-17T12:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052),
            candle("2026-06-17T13:00:00Z", 1.1052, 1.1155, 1.1051, 1.1150), // TP first
            candle("2026-06-17T13:30:00Z", 1.1150, 1.1151, 1.0990, 1.0995),
            candle("2026-06-17T15:00:00Z", 1.0995, 1.1000, 1.0990, 1.0995),
        ];

        // Eager: hand the sim everything.
        let eager = simulate_fill_resolved_zoom(
            &resolved,
            &intent,
            &shell,
            0.0001,
            &path,
            &FakeSubBars(whole_window.clone()),
        );

        // Lazy: pass 1 records, fetch only those windows, pass 2 serves them.
        let rec = RecordingSubBars::new();
        simulate_fill_resolved_zoom(&resolved, &intent, &shell, 0.0001, &path, &rec);
        let windows = rec.windows();
        let fetched: Vec<BidAskCandle> = whole_window
            .iter()
            .filter(|c| windows.iter().any(|w| c.time >= w.start && c.time < w.end))
            .cloned()
            .collect();
        let lazy = simulate_fill_resolved_zoom(
            &resolved,
            &intent,
            &shell,
            0.0001,
            &path,
            &WindowSubBars::new(fetched.clone()),
        );

        assert_eq!(
            format!("{eager:?}"),
            format!("{lazy:?}"),
            "lazy zoom must score identically to the eager pull"
        );
        // ...and it only needed the 2 finer candles inside the ambiguous bar's
        // [13:00, 14:00) window, not all 5. The 11:00/12:00/15:00 bars are
        // exactly the waste the eager pull pays for on every replay.
        assert_eq!(fetched.len(), 2, "fetched {fetched:?}");
        assert!(
            fetched.iter().all(
                |c| c.time >= ts("2026-06-17T13:00:00Z") && c.time < ts("2026-06-17T14:00:00Z")
            ),
            "fetched outside the recorded window: {fetched:?}"
        );
    }

    // ---- Rule 1 + Rule 2 at the ENTRY POINT --------------------------------
    //
    // `core::order_control::in_force_stop` holds the pure rules; these drive
    // `simulate_fill`, `widen_episodes_at_resolved` and `breakeven_armed_at` —
    // the functions the replay actually calls — because a pure-layer test cannot
    // see whether the caller wired the rule up. Every one of these was RED
    // against unmodified production code.

    /// Shared geometry for the Rule 1 / Rule 2 entry-point tests.
    ///
    /// A GBP/AUD **short**: entry 1.1000, placement SL 1.1030, TP **1.0800**.
    /// The 50%-to-TP break-even level is therefore **1.0900**, far from both the
    /// stop and the target — so a bar that closes past it can only *arm or not*,
    /// never exit. (An earlier draft used the default 1.0950 TP; a "past 50%"
    /// close then also touched TP, and the test measured the wrong thing.)
    ///
    /// GBP/AUD carries TN spread-hour mask `[21]`, so the 21:00Z bar is a
    /// spread hour and the 20:00Z bar leads into it — the same gate the live
    /// cron uses.
    fn be_widen_intent() -> (Intent, Shell) {
        use trade_control_core::intent::Breakeven;
        let mut intent = short_stop_intent();
        intent.instrument = "GBP/AUD".into();
        intent.breakeven = Some(Breakeven::at_half());
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.0800,
        }));
        let shell = Shell::from_candle(
            &candle("2026-07-13T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );
        (intent, shell)
    }

    /// The bar the short fills on (bid reaches the 1.1000 sell-stop).
    fn be_fill_bar() -> BidAskCandle {
        ba_candle("2026-07-13T19:00:00Z", 1.1000, 1.0999, 1.1002, 1.1001)
    }

    /// The 20:00Z bar leading into the 21:00Z spread hour. Deliberately quiet:
    /// its ask high **1.09950** stays clear of every stop the trade can be
    /// holding — the 1.1030 placement stop AND the 1.1000 break-even stop — and
    /// its close is nowhere near the 1.0900 arming level, so it neither exits
    /// nor arms under either rule.
    ///
    /// The break-even clearance matters: once Rule 1 is in force the exit test
    /// uses the IN-FORCE stop, so a lead bar poking above 1.1000 ends the scan
    /// before the spread hour is ever reached and the widen silently disappears.
    fn be_lead_bar() -> BidAskCandle {
        ba_candle("2026-07-13T20:00:00Z", 1.09930, 1.09920, 1.09950, 1.09940)
    }

    /// **RULE 1 at the entry point.** An episode that starts AFTER break-even
    /// armed must widen from — and restore to — the break-even stop (the entry
    /// price 1.1000), not the placement stop (1.1030).
    ///
    /// Before the fix `widen_episodes_at_resolved` took every episode's
    /// `original_stop` from `resolved.stop_loss`, frozen at placement, so it
    /// widened from 1.1030 and restored to it — handing back a stop 30 pips
    /// wider than the one the operator had banked, exactly the GBP/ZAR
    /// 2026-07-27 divergence (22.350 vs the banked 22.260).
    ///
    /// This is a **RULE** difference, not a resolution one: no bar granularity
    /// and no sub-bar zoom changes which source number is read.
    #[test]
    fn a_widen_after_break_even_widens_from_the_break_even_stop() {
        let (intent, shell) = be_widen_intent();
        // 18:00Z — QUIET (no spread hour), closes at ~1.0880, past the 1.0900
        // arming level ⇒ break-even arms here, three hours before any widen.
        let arms_be = ba_candle("2026-07-13T18:00:00Z", 1.08800, 1.08790, 1.08810, 1.08800);
        // 21:00Z spread hour, and a 22:00Z recovered bar so the episode restores.
        let spike = ba_candle("2026-07-13T21:00:00Z", 1.08900, 1.08850, 1.08980, 1.08930);
        let recovered = ba_candle("2026-07-13T22:00:00Z", 1.08900, 1.08890, 1.08910, 1.08900);
        let path = [
            fire_bar(),
            be_fill_bar(),
            arms_be,
            be_lead_bar(),
            spike,
            recovered,
        ];

        let resolved = Resolved::from_intent(&intent, &shell, 0.0001, replay_tick(&intent, 0.0001))
            .expect("test bracket resolves");
        // Premise guard: the placement stop really is 1.1030, so "1.1000 not
        // 1.1030" below is a genuine two-value distinction.
        assert!(
            (resolved.stop_loss - 1.1030).abs() < 1e-9,
            "premise: placement stop is 1.1030, got {}",
            resolved.stop_loss
        );
        // Premise guard: break-even really did arm before the spread hour.
        assert_eq!(
            breakeven_armed_at(&intent, &shell, 0.0001, &path, None),
            Some(ts("2026-07-13T18:00:00Z")),
            "premise: break-even arms on the quiet 18:00Z bar"
        );

        let eps = widen_episodes_at_resolved(&resolved, &intent, &shell, 0.0001, &path, 22.0);
        let ep = eps.first().expect("the 21:00Z spread hour must widen");
        assert!(
            (ep.original_stop - 1.1000).abs() < 1e-9,
            "Rule 1: the widen must move the stop IN FORCE (break-even 1.1000), \
             not the placement stop 1.1030; got original_stop {}",
            ep.original_stop,
        );
        // ...and because the widened level is derived from it, it lands below
        // the placement stop rather than ~14 pips above it.
        assert!(
            ep.widened_stop < 1.1030,
            "a widen from break-even lands below the placement stop 1.1030, got {}",
            ep.widened_stop,
        );
    }

    /// The restore half of Rule 1, read at the **exit price** — the number the
    /// corpus scores. After the episode restores, the stop in force is
    /// break-even again, so a bar whose ask reaches 1.1002 (past break-even
    /// 1.1000, well short of the placement stop 1.1030) closes the trade at 0R.
    ///
    /// ⚠️ **This half was already correct before the fix, and the test says so.**
    /// `simulate_fill` asks `WidenEpisodes::stop_on_bar(bar, active_stop)`, and
    /// once an episode no longer covers the bar that call falls through to
    /// `active_stop` — the break-even-managed stop — so the *restore* already
    /// landed on break-even. What was wrong was the number the episode widened
    /// **from** (and therefore the level in force *during* the episode), which is
    /// `a_widen_after_break_even_widens_from_the_break_even_stop`. This test is
    /// kept as a **regression guard on the correct half**: the in-force rewrite
    /// touches exactly this code path, and silently restoring to the placement
    /// stop would be the easiest way to break it.
    ///
    /// The second assertion below — on the level in force *inside* the episode —
    /// is the part that was RED.
    #[test]
    fn the_restore_puts_the_stop_back_to_break_even_not_the_placement_stop() {
        let (intent, shell) = be_widen_intent();
        let arms_be = ba_candle("2026-07-13T18:00:00Z", 1.08800, 1.08790, 1.08810, 1.08800);
        let spike = ba_candle("2026-07-13T21:00:00Z", 1.08900, 1.08850, 1.08980, 1.08930);
        let recovered = ba_candle("2026-07-13T22:00:00Z", 1.08900, 1.08890, 1.08910, 1.08900);
        // 23:00Z — ask high 1.10020: past the restored break-even stop 1.1000,
        // 28 pips short of the placement stop 1.1030.
        let reaches_break_even =
            ba_candle("2026-07-13T23:00:00Z", 1.09000, 1.08900, 1.10020, 1.08950);
        let path = [
            fire_bar(),
            be_fill_bar(),
            arms_be,
            be_lead_bar(),
            spike,
            recovered,
            reaches_break_even,
        ];

        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut {
                exit_price,
                entry_price,
                ..
            } => {
                assert!(
                    (exit_price - 1.1000).abs() < 1e-9,
                    "must exit at the RESTORED break-even stop 1.1000, got {exit_price}"
                );
                assert!(
                    (exit_price - entry_price).abs() < 1e-9,
                    "which is a 0R scratch — exit == entry"
                );
            }
            other => panic!(
                "the restored break-even stop must close the trade; got {other:?} \
                 (a restore to the placement stop 1.1030 leaves it open)"
            ),
        }

        // The half that WAS red: the stop in force DURING the episode is the
        // widen of break-even, not the widen of the placement stop. A widen of
        // 1.1030 sits above 1.1030; a widen of 1.1000 sits below it.
        let resolved = Resolved::from_intent(&intent, &shell, 0.0001, replay_tick(&intent, 0.0001))
            .expect("resolves");
        let eps = widen_episodes_at_resolved(&resolved, &intent, &shell, 0.0001, &path, 22.0);
        let ep = eps.first().expect("the 21:00Z spread hour widens");
        assert_eq!(
            ep.effective_from,
            ts("2026-07-13T20:00:00Z"),
            "premise: the 30-min lead makes the LEAD bar the one that opens the \
             episode, so that is the bar Rule 2 has to shield"
        );
        assert!(
            ep.widened_stop < 1.1030,
            "the level in force during the episode must be a widen of break-even \
             (1.1000), so below the placement stop 1.1030; got {}",
            ep.widened_stop
        );
    }

    /// **RULE 2 at the entry point — the operator's required sequence.**
    ///
    /// 1. a widen happens,
    /// 2. price goes more than half way to TP DURING the widen,
    /// 3. the widen ends (restore).
    ///
    /// Break-even must NOT have armed from the mid-widen bar, and MUST arm from
    /// a qualifying bar after the restore.
    ///
    /// The evidence is the **exit price**, which is what the corpus scores. In
    /// the first path a later bar reaches the placement stop 1.1030 ⇒ −1R; had
    /// the mid-widen bar armed break-even, that same bar would have exited at
    /// 1.1000 ⇒ 0R. Two different numbers, so the assertion cannot pass under
    /// the wrong rule.
    #[test]
    fn break_even_does_not_arm_inside_a_widen_but_does_after_the_restore() {
        let (intent, shell) = be_widen_intent();
        // (1)+(2) 21:00Z — INSIDE the widen, closing at ~1.0880, well past the
        // 1.0900 arming level. Its ask high 1.08810 is clear of every stop and
        // its bid low 1.08790 is clear of the 1.0800 TP, so the only thing this
        // bar can do is arm or not.
        let mid_widen = ba_candle("2026-07-13T21:00:00Z", 1.08800, 1.08790, 1.08810, 1.08800);
        // (3) 22:00Z — spread recovered (≤4p) ⇒ the widen restores here. Closes
        // at ~1.0990, ABOVE the 1.0900 level, so this bar does not arm either.
        let restore = ba_candle("2026-07-13T22:00:00Z", 1.09900, 1.09890, 1.09910, 1.09900);
        // 23:00Z — ask high 1.10310 reaches the PLACEMENT stop 1.1030.
        let hits_placement_stop =
            ba_candle("2026-07-13T23:00:00Z", 1.09950, 1.09900, 1.10310, 1.09980);
        let path = [
            fire_bar(),
            be_fill_bar(),
            be_lead_bar(),
            mid_widen,
            restore,
            hits_placement_stop,
        ];

        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!(
                    (exit_price - 1.1030).abs() < 1e-9,
                    "Rule 2: the mid-widen close must NOT arm break-even, so the \
                     stop is still the placement stop 1.1030; got {exit_price} \
                     (1.1000 means the rubbish candle armed it)"
                );
            }
            other => panic!("expected a stop-out at the placement stop, got {other:?}"),
        }

        // The SECOND half — a qualifying bar AFTER the restore does arm. Same
        // prefix, but instead of the stop-out bar: a 00:00Z bar that closes past
        // 1.0900 (a fresh reading, outside any widen), then a 01:00Z bar that
        // reaches break-even 1.1000 only.
        let arms_after_restore =
            ba_candle("2026-07-14T00:00:00Z", 1.08800, 1.08790, 1.08810, 1.08800);
        let reaches_break_even =
            ba_candle("2026-07-14T01:00:00Z", 1.09000, 1.08900, 1.10020, 1.08950);
        let path2 = [
            fire_bar(),
            be_fill_bar(),
            be_lead_bar(),
            mid_widen,
            restore,
            arms_after_restore,
            reaches_break_even,
        ];
        match simulate_fill(&intent, &shell, 0.0001, &path2) {
            SimOutcome::StoppedOut {
                exit_price,
                entry_price,
                ..
            } => {
                assert!(
                    (exit_price - 1.1000).abs() < 1e-9,
                    "a qualifying bar AFTER the restore takes a fresh reading and \
                     arms break-even → exit at entry 1.1000, got {exit_price}"
                );
                assert!((exit_price - entry_price).abs() < 1e-9, "0R scratch");
            }
            other => panic!("expected a break-even scratch after the restore, got {other:?}"),
        }
    }

    /// The journal line must agree with the scored outcome. `breakeven_armed_at`
    /// is a **second** walk of the candle path (the report's "SL→break-even"
    /// line), so Rule 2 has to reach it too — otherwise the journal claims
    /// break-even armed at 21:00 on a bar the simulator refused, and the operator
    /// reads a trace that contradicts its own R.
    #[test]
    fn the_breakeven_journal_line_also_skips_mid_widen_bars() {
        let (intent, shell) = be_widen_intent();
        let mid_widen = ba_candle("2026-07-13T21:00:00Z", 1.08800, 1.08790, 1.08810, 1.08800);
        let restore = ba_candle("2026-07-13T22:00:00Z", 1.09900, 1.09890, 1.09910, 1.09900);
        let arms_after = ba_candle("2026-07-14T00:00:00Z", 1.08800, 1.08790, 1.08810, 1.08800);
        let path = [
            fire_bar(),
            be_fill_bar(),
            be_lead_bar(),
            mid_widen,
            restore,
            arms_after,
        ];

        assert_eq!(
            breakeven_armed_at(&intent, &shell, 0.0001, &path, None),
            Some(ts("2026-07-14T00:00:00Z")),
            "the journal must report the POST-RESTORE arming bar, not the \
             21:00Z rubbish candle inside the widen"
        );
    }

    /// Premise guard for every Rule 2 test above: the SAME arming close, on a
    /// bar **no** widen covers, DOES arm. Without this, a rule that accidentally
    /// suppressed break-even everywhere — or a bar that silently failed to
    /// qualify for some unrelated reason — would pass all of them.
    ///
    /// The ordinary bar is at **10:00Z**, not 21:00Z: GBP/AUD's mask flags 21:00
    /// and the legacy NY-close-edge fallback *also* flags 21:00Z in July (17:00
    /// EDT), so 21:00Z is a spread hour on essentially every instrument. Only a
    /// mid-session hour is genuinely clear, and the guard is worthless unless it
    /// is.
    #[test]
    fn the_same_close_arms_break_even_when_no_widen_covers_it() {
        let (intent, shell) = be_widen_intent();
        // Same 1.0880 close as the Rule 2 tests, on a mid-session bar.
        let fill_bar = ba_candle("2026-07-13T08:00:00Z", 1.1000, 1.0999, 1.1002, 1.1001);
        let quiet = ba_candle("2026-07-13T09:00:00Z", 1.09930, 1.09920, 1.09950, 1.09940);
        let same_bar = ba_candle("2026-07-13T10:00:00Z", 1.08800, 1.08790, 1.08810, 1.08800);
        let path = [fire_bar(), fill_bar, quiet, same_bar];
        assert_eq!(
            breakeven_armed_at(&intent, &shell, 0.0001, &path, None),
            Some(ts("2026-07-13T10:00:00Z")),
            "premise guard: with no widen covering it, this close arms normally"
        );
    }

    /// The bar that **opens** an episode is itself inside it, so under Rule 2 it
    /// does not arm break-even — and the widen it opens must therefore widen from
    /// the PRE-arm stop.
    ///
    /// This is the one bar the two replay walks can disagree about.
    /// `simulate_fill` asks `WidenEpisodes::covers`, which is true at
    /// `effective_from`, so it refuses. `widen_episodes_at_resolved` builds the
    /// episodes and so has no episode to ask about *yet* when it reaches that
    /// bar — an earlier draft armed there and then widened from the just-armed
    /// level, making the two walks report different `original_stop`s for the same
    /// trade.
    ///
    /// The fixture's 21:00Z bar both (a) starts the spread hour and (b) closes
    /// past the 1.0900 arming level, so the two behaviours give different
    /// numbers: widen-from-1.1030 (correct) vs widen-from-1.1000 (the bug).
    #[test]
    fn the_bar_that_opens_an_episode_does_not_arm_break_even() {
        let (intent, shell) = be_widen_intent();
        // The bar that OPENS the episode is the **20:00Z lead bar**, not the
        // 21:00Z spread hour itself: `spread_hour_widen_instant` gives the live
        // cron's 30-minute lead, so `effective_from` is the lead bar's open. So
        // the lead bar is the one that must be prevented from arming — and this
        // fixture makes it a bar that otherwise WOULD (close ~1.0880, past the
        // 1.0900 level). An earlier draft of this test put the qualifying close
        // on the 21:00Z bar, which `effective_from` had already shielded, and the
        // test passed against the very bug it was written for.
        let lead_opens_and_would_arm =
            ba_candle("2026-07-13T20:00:00Z", 1.08800, 1.08790, 1.08810, 1.08800);
        let spike = ba_candle("2026-07-13T21:00:00Z", 1.08900, 1.08850, 1.08980, 1.08930);
        let recovered = ba_candle("2026-07-13T22:00:00Z", 1.09900, 1.09890, 1.09910, 1.09900);
        let path = [
            fire_bar(),
            be_fill_bar(),
            lead_opens_and_would_arm,
            spike,
            recovered,
        ];

        let resolved = Resolved::from_intent(&intent, &shell, 0.0001, replay_tick(&intent, 0.0001))
            .expect("resolves");
        let eps = widen_episodes_at_resolved(&resolved, &intent, &shell, 0.0001, &path, 22.0);
        let ep = eps.first().expect("the 21:00Z spread hour widens");
        assert!(
            (ep.original_stop - 1.1030).abs() < 1e-9,
            "the bar opening the episode must not have armed break-even, so the \
             widen is measured from the placement stop 1.1030; got {}",
            ep.original_stop
        );
        // ...and the journal walk must agree that nothing armed on that bar.
        assert_eq!(
            breakeven_armed_at(&intent, &shell, 0.0001, &path, None),
            None,
            "the journal must not report an arm on the bar that opens the widen"
        );
    }
}
