//! Format the replay outcome: each fire, and — for enters — the simulated fill.
//!
//! The shell-from-fire reconstruction mirrors the worker's `dispatch_fired`
//! (an H&S Pine fire folds its latched signal onto the shell; everything else
//! gets the plain candle shell), so `simulate_fill` resolves entry/SL/TP against
//! the same levels the live dispatch would have.
//!
//! ## Reversal-close post-pass
//!
//! `simulate_fill` is pure and per-enter: it knows only the bracket (entry / SL
//! / TP) and the forward candle path. The `06-close-on-reversal` close is a
//! *separate* fire (a `PinePattern` guard the engine now fires when a confirming
//! opposite-direction reversal candle prints inside the SR band). So the report
//! runs a small post-pass — [`apply_reversal_close`] — that flattens an open
//! position on the earliest reversal-close fire that lands while it's open,
//! before its SL/TP. The engine decides *whether* the reversal fires (shared
//! with the worker); this layer only *applies* it to the simulated position,
//! since the worker's real `run_close` (which the offline replay doesn't run)
//! is what flattens the broker position live.

use chrono::{DateTime, Utc};
use trade_control_core::intent::{
    Action, Direction, NoEntryWindow, Resolved, ResolvedEntry, Shell,
};
use trade_control_core::spread_blackout::elevated_threshold_pips;
use trade_control_engine::{
    SimOutcome, SweepReason, TradePlan, breakeven_armed_at, simulate_fill, sweep_reason,
    widened_stop_at,
};

use super::brisbane::bne;
use super::replay::{Fire, Replay};

/// A fired `06-close-on-reversal` close, reduced to what the fill resolution
/// needs: the bar it fired on and the price the position flattens at. The
/// worker's `run_close` flattens at market when the reversal candle prints, so
/// the bar's **close** is the faithful exit-price proxy (the engine fires the
/// close on that bar's close, and the worker dispatches it that tick).
#[derive(Debug, Clone, Copy)]
pub struct CloseFire {
    pub at: DateTime<Utc>,
    pub price: f64,
}

/// Whether the worker's `allow_close` gate would let this close flatten the
/// position. The live worker runs the shared
/// [`trade_control_core::allow_close_gate::evaluate`] on every `run_close`; a
/// non-`Proceed` outcome (`allow_close` false, a script error, or an unmet
/// `needs_golden` / `needs_confirmed`) is a 412 that leaves the position OPEN.
/// Without this the replay would close it and diverge from the live worker.
fn close_gate_passes(fire: &Fire) -> bool {
    let intent = &fire.fired.intent;
    let candle = &fire.fired.candle;
    let shell = match &fire.fired.signal {
        Some(sig) => Shell::from_candle_and_signal(candle, sig),
        None => Shell::from_candle(candle),
    };
    matches!(
        trade_control_core::allow_close_gate::evaluate(intent, &shell),
        trade_control_core::allow_close_gate::AllowCloseOutcome::Proceed
    )
}

/// Every `Action::Close` fire in the replay **whose `allow_close` gate passes**,
/// in fire order, reduced to [`CloseFire`]s. The fill resolution consults these
/// to exit an open position on a reversal candle — the engine now fires the
/// close (a `PinePattern` guard), but the pure per-enter `simulate_fill` only
/// knows SL/TP, so the replay must apply the close itself. A close the
/// `allow_close` gate would block is dropped here so the position stays open
/// (matching the live worker); the blocked close still renders its own
/// `BLOCKED` line via [`render_fire`].
pub fn collect_close_fires(replay: &Replay) -> Vec<CloseFire> {
    replay
        .fires
        .iter()
        .filter(|f| f.fired.intent.action == Action::Close)
        .filter(|f| close_gate_passes(f))
        .map(|f| CloseFire {
            at: f.fired.candle.time,
            price: f.fired.candle.c,
        })
        .collect()
}

/// Apply any reversal-close to a per-enter [`SimOutcome`]. A close fire flattens
/// the position when its bar lands **after the fill** and **at-or-before** the
/// outcome's own exit — i.e. the reversal printed while the trade was still
/// open. The earliest such close wins (a position only closes once).
///
/// - `FilledOpen` (no SL/TP touched) → closed on the first post-fill reversal.
/// - `StoppedOut` / `TookProfit` → only overridden if a reversal fires *strictly
///   before* that exit bar; a close on/after the SL/TP bar is moot (the position
///   was already flat). Ties go to the SL/TP (the simulator's pessimistic
///   stance, and the close can't pre-empt an exit that already happened).
/// - `NeverFilled` / `Declined` / `SpreadBlackout` / `Unresolved` → untouched
///   (no open position).
///
/// Returns the (possibly overridden) outcome; on override it's the new
/// [`ReplayOutcome::ClosedOnReversal`] carrying the close bar + price.
fn apply_reversal_close(outcome: SimOutcome, closes: &[CloseFire]) -> ReplayOutcome {
    let (fill_at, entry_price, exit_limit) = match &outcome {
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
        | SimOutcome::Unresolved(_) => {
            return ReplayOutcome::Sim(outcome);
        }
    };

    let reversal = closes
        .iter()
        .filter(|c| c.at > fill_at)
        .filter(|c| match exit_limit {
            // Only a reversal strictly before the SL/TP bar pre-empts it.
            Some(exit_at) => c.at < exit_at,
            None => true,
        })
        .min_by_key(|c| c.at);

    match reversal {
        Some(c) => ReplayOutcome::ClosedOnReversal {
            fill_at,
            entry_price,
            exit_at: c.at,
            exit_price: c.price,
        },
        None => ReplayOutcome::Sim(outcome),
    }
}

/// A per-enter fill outcome after the replay's reversal-close post-pass: either
/// the pure simulator's verdict ([`SimOutcome`]) or a reversal close that
/// flattened the position before its SL/TP/window-end.
#[derive(Debug, Clone)]
enum ReplayOutcome {
    Sim(SimOutcome),
    /// The position was flattened by a `06-close-on-reversal` fire while open.
    ClosedOnReversal {
        fill_at: DateTime<Utc>,
        entry_price: f64,
        exit_at: DateTime<Utc>,
        exit_price: f64,
    },
}

/// How a position resolved, for annotation purposes. The first three are
/// *taken* (filled) outcomes; the last two are *not-taken* — an order that
/// the price path never filled, or an entry the worker declined to place.
/// `--annotate` always draws the taken outcomes; `--annotate-unfilled` adds
/// the not-taken ones (drawn at the fire bar, muted, since there is no fill).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillKind {
    /// Filled and still open at the window's end.
    Open,
    /// Filled then hit stop-loss.
    StoppedOut,
    /// Filled then hit take-profit.
    TookProfit,
    /// Filled, then flattened by a `06-close-on-reversal` fire (a confirming
    /// opposite-direction reversal candle inside the SR band) before SL/TP.
    ClosedOnReversal,
    /// A pending order that never triggered within the window. Not taken.
    NeverFilled,
    /// An entry the worker declined to place (entry past a gate level). Not taken.
    Declined,
    /// An entry the worker's spread-blackout gate would have rejected (fire-bar
    /// spread above threshold inside the NY-close-edge window). Not taken.
    SpreadBlackout,
    /// An entry the worker's pre-broker gate would have rejected — the
    /// `allow_entry` script returned false/errored, or the candle-quality
    /// requirement (`needs_golden` / `needs_confirmed`) the signal-folded shell
    /// didn't meet. Not taken (the live worker 412s before placing the order).
    GateBlocked,
}

impl FillKind {
    /// Was this position actually taken (an order filled)? `false` for the
    /// not-taken kinds (`NeverFilled` / `Declined` / `SpreadBlackout`), which
    /// only have an *intended* bracket, anchored at the fire bar.
    pub fn is_taken(self) -> bool {
        matches!(
            self,
            Self::Open | Self::StoppedOut | Self::TookProfit | Self::ClosedOnReversal
        )
    }
}

/// The resolved bracket plus simulated fill for one *filled* enter fire —
/// everything `--annotate` needs to draw the position. Computed by
/// [`resolve_fire`] so the report line and the annotator agree on the
/// same levels (the same resolution the report's order line uses).
#[derive(Debug, Clone)]
pub struct FireResult {
    pub direction: Direction,
    /// Open-time of the bar the entry actually filled on (UTC).
    pub fill_at: DateTime<Utc>,
    /// Right-edge time anchor for the drawn box: the exit bar for a closed
    /// trade, or the last replayed bar for one still open at window end.
    pub until: DateTime<Utc>,
    /// The placed entry level the fill happened at.
    pub entry_price: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub kind: FillKind,
}

/// Resolve the bracket + simulate the fill for one fire, returning a
/// [`FireResult`] **only** when it's an enter that actually filled
/// (`Open` / `StoppedOut` / `TookProfit`). Non-enters, unresolved
/// brackets, declined entries, and never-filled pending orders yield
/// `None` — they have no *taken* position to annotate.
///
/// The shell reconstruction mirrors the worker's dispatch (an H&S Pine
/// fire folds its latched signal onto the shell), so the levels match
/// what the live worker would have placed — and the report's `order:`
/// line, which resolves the same way.
pub fn resolve_fire(plan: &TradePlan, fire: &Fire, closes: &[CloseFire]) -> Option<FireResult> {
    resolve_fire_any(plan, fire, closes).filter(|r| r.kind.is_taken())
}

/// Like [`resolve_fire`], but also returns the *not-taken* enters —
/// pending orders that never filled (`NeverFilled`) and entries the worker
/// declined (`Declined`). These have no fill bar, so the box is anchored at
/// the **fire bar** (where the order would have been placed) and runs to the
/// window end; `entry_price` is the intended placed level, not a fill.
///
/// Still `None` for non-enters and for enters whose bracket can't resolve
/// (an `Unresolved` outcome — nothing meaningful to draw).
pub fn resolve_fire_any(plan: &TradePlan, fire: &Fire, closes: &[CloseFire]) -> Option<FireResult> {
    let intent = &fire.fired.intent;
    if intent.action != Action::Enter {
        return None;
    }
    // A superseded enter never went on (its resting order was cancelled by a
    // later entry before it could fill), so there's no position to annotate —
    // not even a not-taken intended bracket: the cancel-and-replace means the
    // *replacing* entry is the one that carries the bracket forward.
    if fire.superseded {
        return None;
    }
    let candle = &fire.fired.candle;
    let shell = match &fire.fired.signal {
        Some(sig) => Shell::from_candle_and_signal(candle, sig),
        None => Shell::from_candle(candle),
    };
    let resolved = Resolved::from_intent(intent, &shell, plan.pip_size).ok()?;
    // For an open / not-taken trade the box runs to the last replayed bar;
    // closed trades override this with their exit bar below.
    let window_end = fire.forward.last().map(|c| c.time)?;
    // The fire bar is the not-taken anchor: an order that never filled (or was
    // declined) was still "placed" at this bar, so draw the intended bracket
    // there.
    let fire_at = candle.time;

    // The entry decision was made ONCE in the tick loop by the REAL `run_enter`
    // (every gate: pause / retry / cooldown / prep / veto / entry-level-veto /
    // allow_entry / blackouts / SL-floor). A `Rejected` enter is not-taken — the
    // live worker 412/422/423s it before placing an order — so anchor the
    // intended bracket at the fire bar as a `GateBlocked` and tally 0R. This
    // path no longer re-derives any gate; the verdict comes off the fire.
    if fire.rejected_reason().is_some() {
        tracing::debug!(
            bar = %candle.time,
            reason = ?fire.rejected_reason(),
            "entry rejected by run_enter — annotation path, not taken (0R)"
        );
        return Some(FireResult {
            direction: resolved.direction,
            fill_at: fire_at,
            until: window_end,
            entry_price: resolved.entry.reference_price(),
            stop_loss: resolved.stop_loss,
            take_profit: resolved.take_profit,
            kind: FillKind::GateBlocked,
        });
    }

    let raw = simulate_fill(intent, &shell, plan.pip_size, &fire.forward);
    let (fill_at, until, entry_price, kind) = match apply_reversal_close(raw, closes) {
        ReplayOutcome::ClosedOnReversal {
            fill_at,
            exit_at,
            entry_price,
            ..
        } => (fill_at, exit_at, entry_price, FillKind::ClosedOnReversal),
        ReplayOutcome::Sim(sim) => match sim {
            SimOutcome::FilledOpen {
                fill_at,
                entry_price,
            } => (fill_at, window_end, entry_price, FillKind::Open),
            SimOutcome::StoppedOut {
                fill_at,
                entry_price,
                exit_at,
                ..
            } => (fill_at, exit_at, entry_price, FillKind::StoppedOut),
            SimOutcome::TookProfit {
                fill_at,
                entry_price,
                exit_at,
                ..
            } => (fill_at, exit_at, entry_price, FillKind::TookProfit),
            // Not taken: anchor the intended bracket at the fire bar, running
            // to the window end. The placed level (`resolved`) is the entry.
            SimOutcome::NeverFilled => (
                fire_at,
                window_end,
                resolved.entry.reference_price(),
                FillKind::NeverFilled,
            ),
            SimOutcome::Declined { .. } => (
                fire_at,
                window_end,
                resolved.entry.reference_price(),
                FillKind::Declined,
            ),
            SimOutcome::SpreadBlackout { .. } => (
                fire_at,
                window_end,
                resolved.entry.reference_price(),
                FillKind::SpreadBlackout,
            ),
            // Nothing meaningful to draw.
            SimOutcome::Unresolved(_) => return None,
        },
    };
    Some(FireResult {
        direction: resolved.direction,
        fill_at,
        until,
        entry_price,
        stop_loss: resolved.stop_loss,
        take_profit: resolved.take_profit,
        kind,
    })
}

/// Render the full replay report as a string. When `verbose` is set, a
/// bar-by-bar trace of the engine's silent state changes (phase moves, the
/// break-and-close / retest stamps, fires) is printed first — the events the
/// per-fire report below can't show (notably the retest, which never fires an
/// intent).
pub fn render(
    plan: &TradePlan,
    replay: &Replay,
    simulate: bool,
    verbose: bool,
    blackout_windows: &[NoEntryWindow],
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Plan {} ({}, {:?}) — {} fire(s) over the window\n",
        plan.trade_id,
        plan.instrument,
        plan.granularity,
        replay.fires.len()
    ));

    if verbose {
        out.push_str(&render_trace(replay));
    }

    let closes = collect_close_fires(replay);
    let mut wins = 0usize;
    let mut losses = 0usize;
    let mut reversal_closes = 0usize;
    for fire in &replay.fires {
        out.push_str(&render_fire(
            plan,
            fire,
            simulate,
            &closes,
            blackout_windows,
            &mut wins,
            &mut losses,
            &mut reversal_closes,
        ));
    }

    if !replay.warnings.is_empty() {
        out.push_str("\nWarnings:\n");
        for w in &replay.warnings {
            out.push_str(&format!("  - {w}\n"));
        }
    }

    out.push_str(&format!(
        "\nDone: {}  |  final phase: {:?}  |  fires: {}",
        replay.done,
        replay.final_state.phase,
        replay.fires.len()
    ));
    if simulate {
        out.push_str(&format!("  |  TP: {wins}  SL: {losses}"));
        if reversal_closes > 0 {
            out.push_str(&format!("  REV: {reversal_closes}"));
        }
    }
    out.push('\n');
    out
}

/// Render the `--verbose` bar-by-bar trace: every live bar on which the engine
/// did something (a phase move, a break-and-close / retest stamp, or a fire),
/// in order. Quiet bars are omitted. Returns a short note when no bar was
/// noteworthy (e.g. a window that only ever seeded), so `--verbose` is never
/// silently empty.
fn render_trace(replay: &Replay) -> String {
    let mut out = String::from("\nBar-by-bar engine trace (--verbose):\n");
    let mut any = false;
    for trace in &replay.traces {
        let block = trace.render();
        if !block.is_empty() {
            out.push_str(&block);
            any = true;
        }
    }
    if !any {
        out.push_str("  (no phase moves, stamps, or fires — every live bar seeded silently)\n");
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn render_fire(
    plan: &TradePlan,
    fire: &Fire,
    simulate: bool,
    closes: &[CloseFire],
    blackout_windows: &[NoEntryWindow],
    wins: &mut usize,
    losses: &mut usize,
    reversal_closes: &mut usize,
) -> String {
    let intent = &fire.fired.intent;
    let candle = &fire.fired.candle;
    let mut line = format!(
        "\n• {} {:?} @ {}  close={}\n",
        fire.fired.rule_id,
        intent.action,
        bne(candle.time),
        candle.c
    );

    if !simulate {
        return line;
    }
    if intent.action == Action::Prep {
        // A prep carries no bracket of its own — preview the plan's *enter*
        // bracket resolved against this prep bar, so the journal shows the
        // SL/TP the eventual order would carry even when it never fills.
        line.push_str(&prep_would_enter_line(plan, fire));
        return line;
    }
    if intent.action == Action::Close {
        // A close fire has no fill of its own — it flattens whatever enter is
        // open. The worker's `allow_close` gate (shared core) can block it: a
        // blocked close keeps the position OPEN, so it must NOT flatten the
        // simulated enter (it's already excluded from `collect_close_fires`).
        // Surface either case so the reversal exit is legible next to the enter
        // it closes (which renders its own `CLOSED ON REVERSAL` fill line when
        // the gate passes).
        if close_gate_passes(fire) {
            line.push_str(&format!(
                "    close-on-reversal: reversal candle @ {} (close {}) — flattens the open position\n",
                bne(candle.time),
                candle.c
            ));
        } else {
            line.push_str(&format!(
                "    close: BLOCKED by allow_close gate (position stays open) @ {} (close {})\n",
                bne(candle.time),
                candle.c
            ));
        }
        return line;
    }
    if intent.action != Action::Enter {
        line.push_str("    (non-enter fire — no fill simulated)\n");
        return line;
    }

    // Reconstruct the shell exactly as the worker's dispatch would, so the
    // simulator resolves the same entry/SL/TP levels.
    let shell = match &fire.fired.signal {
        Some(sig) => Shell::from_candle_and_signal(candle, sig),
        None => Shell::from_candle(candle),
    };

    // Resolve the bracket the worker would have placed (direction, entry, SL,
    // TP) and print it for every enter — even ones the price path never fills.
    // This is the journaling line: what order went on at this bar.
    match Resolved::from_intent(intent, &shell, plan.pip_size) {
        Ok(resolved) => line.push_str(&format!(
            "    {}\n",
            describe_order(&resolved, plan.pip_size)
        )),
        Err(err) => line.push_str(&format!("    order: UNRESOLVED — {err:?}\n")),
    }

    // A superseded enter had its resting order cancelled by a later entry
    // (cancel-and-replace) before it could fill, so its standalone simulated
    // fill is fiction — report the cancellation instead and don't tally it.
    // (Checked before the rejection branch: a placed-then-superseded enter has a
    // `Placed` outcome, not a rejection, so the gate verdict wouldn't catch it.)
    if fire.superseded {
        line.push_str("    fill: SUPERSEDED — resting order cancelled by a later entry (cancel-and-replace)\n");
        return line;
    }

    // The entry decision came from the REAL `run_enter` in the tick loop. A
    // `Rejected` enter is a 0R skip — the live worker 412/422/423s it before
    // placing an order — so surface the real dispatch reason and don't simulate
    // a fill or tally it. A pause rejection keeps the legible "SUPPRESSED"
    // wording; every other gate (cooldown / prep / veto / entry-level-veto /
    // allow_entry / blackouts / SL-floor) prints its own reason verbatim.
    if let Some(reason) = fire.rejected_reason() {
        let detail = if reason.starts_with("rejected: paused") {
            format!("fill: SUPPRESSED — trade paused by news blackout [{reason}] → NO FILL / 0R")
        } else {
            format!("fill: BLOCKED — {reason} → NO FILL / 0R")
        };
        line.push_str(&format!("    {detail}\n"));
        return line;
    }

    // Break-even arming: the bar whose close runs past the threshold
    // (50%-to-TP by default). In production the live cron (`breakeven_watch`)
    // sends `amend_stop(entry)` to the broker on the tick that observes this
    // candle; surface it so the journal shows *when* the SL moved to break-even
    // (otherwise a BREAK-EVEN stop-out below looks like it came from nowhere).
    if let Some(armed_at) = breakeven_armed_at(intent, &shell, plan.pip_size, &fire.forward) {
        line.push_str(&format!(
            "    be: SL→break-even @ {} (a candle closed past 50%-to-TP; live cron amends the broker SL here)\n",
            bne(armed_at)
        ));
    }

    // Spread-widen (System 2): a post-fill bar whose spread reaches the widen
    // trigger moves the broker SL *away* from price (live cron
    // `blackout_apply`). A widened stop changes the exit price, so surface it —
    // otherwise a wider-than-bracket stop-out below looks wrong. The trigger is
    // the instrument's *real* spread-blackout threshold (`baseline × 5`), now
    // that the baked baseline lives in shared `core` and the engine links it —
    // the same `elevated_threshold_pips` the System-1 entry-reject uses, so the
    // two stay exact and in lockstep with the live worker.
    let widen_trigger = elevated_threshold_pips(&intent.instrument);
    if let Some(widen) =
        widened_stop_at(intent, &shell, plan.pip_size, &fire.forward, widen_trigger)
    {
        line.push_str(&format!(
            "    exit: SL widened to {} (spread blackout System 2) @ {} (from {})\n",
            fmt_price(widen.widened_stop, plan.pip_size),
            bne(widen.at),
            fmt_price(widen.original_stop, plan.pip_size)
        ));
    }

    let raw = simulate_fill(intent, &shell, plan.pip_size, &fire.forward);
    match apply_reversal_close(raw, closes) {
        ReplayOutcome::ClosedOnReversal {
            entry_price,
            exit_at,
            exit_price,
            ..
        } => {
            line.push_str(&format!(
                "    fill: CLOSED ON REVERSAL — in @ {entry_price} → exit {exit_price} ({})\n",
                bne(exit_at)
            ));
            *reversal_closes += 1;
        }
        ReplayOutcome::Sim(outcome) => {
            // A `NeverFilled` order isn't necessarily one that passively never
            // triggered — the live cron sweep actively cancels a still-resting
            // order once its window expires, its bar-expiry passes, or price
            // overtakes its SL. Surface *why* the worker would have swept it so
            // the replay distinguishes a swept order from an untriggered one.
            let detail = match &outcome {
                SimOutcome::NeverFilled => {
                    let swept = sweep_reason(
                        intent,
                        &shell,
                        plan.pip_size,
                        &fire.forward,
                        blackout_windows,
                    );
                    describe_never_filled(swept)
                }
                other => describe_outcome(other),
            };
            line.push_str(&format!("    {detail}\n"));
            match outcome {
                SimOutcome::TookProfit { .. } => *wins += 1,
                SimOutcome::StoppedOut { .. } => *losses += 1,
                _ => {}
            }
        }
    }
    line
}

/// One-line summary of the bracket order the worker would have placed: the
/// direction, the order type + entry level, and the SL / TP. Printed for every
/// enter fire so the report doubles as a journaling record — including orders
/// that the price path never fills. Prices are rounded to the instrument's
/// price grid (`pip_size`) so resolved levels read cleanly instead of carrying
/// binary-float noise (e.g. `0.46367`, not `0.46366999999999997`).
fn describe_order(resolved: &Resolved, pip_size: f64) -> String {
    let dir = match resolved.direction {
        Direction::Long => "LONG",
        Direction::Short => "SHORT",
    };
    let entry = match resolved.entry {
        ResolvedEntry::Market { reference_price } => {
            format!("market @ {}", fmt_price(reference_price, pip_size))
        }
        ResolvedEntry::Stop { trigger_price } => {
            format!("stop @ {}", fmt_price(trigger_price, pip_size))
        }
        ResolvedEntry::Limit { trigger_price } => {
            format!("limit @ {}", fmt_price(trigger_price, pip_size))
        }
    };
    format!(
        "order: {dir} {entry}  SL {}  TP {}",
        fmt_price(resolved.stop_loss, pip_size),
        fmt_price(resolved.take_profit, pip_size)
    )
}

/// The `would-enter:` preview line for a prep fire: resolve the plan's
/// *enter* bracket against the prep bar's shell and show its direction /
/// entry / SL / TP. A prep gates the entry but carries no bracket itself,
/// so this previews what order the setup is building toward — useful for
/// journaling even when the enter never fires or fills.
///
/// Falls back to a short note when the plan has no enter rule or the
/// enter can't resolve at this bar (e.g. a PinePattern enter that needs a
/// latched signal the prep bar doesn't yet have).
fn prep_would_enter_line(plan: &TradePlan, fire: &Fire) -> String {
    let Some(enter) = plan_enter_intent(plan) else {
        return "    (prep — plan has no enter rule to preview)\n".to_string();
    };
    let candle = &fire.fired.candle;
    let shell = match &fire.fired.signal {
        Some(sig) => Shell::from_candle_and_signal(candle, sig),
        None => Shell::from_candle(candle),
    };
    match Resolved::from_intent(enter, &shell, plan.pip_size) {
        Ok(resolved) => format!(
            "    would-enter: {}\n",
            describe_order(&resolved, plan.pip_size)
                .strip_prefix("order: ")
                .unwrap_or("?")
        ),
        Err(_) => "    (prep — enter bracket not resolvable at this bar yet)\n".to_string(),
    }
}

/// The first enter intent in the plan, if any. The bundle has exactly one
/// enter rule (`05-enter`); the rest are vetos/preps.
fn plan_enter_intent(plan: &TradePlan) -> Option<&trade_control_core::intent::Intent> {
    plan.rules
        .iter()
        .map(|r| &r.intent)
        .find(|i| i.action == Action::Enter)
}

/// Format a price at the instrument's display precision: one decimal place
/// finer than `pip_size` (a fractional-pip digit), which covers FX 5/3-dp
/// quotes and index whole-point grids alike. Falls back to 5 decimals when
/// `pip_size` is non-positive.
fn fmt_price(price: f64, pip_size: f64) -> String {
    let decimals = if pip_size > 0.0 {
        // pip_size 0.0001 → 4 pip decimals → 5 shown; 1.0 → 0 → 1 shown.
        (-pip_size.log10()).round().max(0.0) as usize + 1
    } else {
        5
    };
    format!("{price:.decimals$}")
}

/// The `NEVER FILLED` line, annotated with the sweep reason when the live cron
/// would have actively cancelled the resting order (vs it simply never
/// triggering). `swept` is `sweep_reason`'s verdict: `None` keeps the plain
/// wording (the order simply never triggered, or no market-hours window source
/// was available so the blackout branch couldn't fire).
fn describe_never_filled(swept: Option<(SweepReason, DateTime<Utc>)>) -> String {
    match swept {
        Some((SweepReason::SlBreached, at)) => format!(
            "fill: NEVER FILLED — swept: SL breached @ {} (live cron cancels the resting order here)",
            bne(at)
        ),
        Some((SweepReason::BarExpiry, at)) => {
            format!("fill: NEVER FILLED — swept: bar-expiry @ {}", bne(at))
        }
        Some((SweepReason::Expired, at)) => {
            format!("fill: NEVER FILLED — alert-window expired @ {}", bne(at))
        }
        Some((SweepReason::Blackout, at)) => format!(
            "fill: NEVER FILLED — swept: market-hours blackout @ {} (live cron cancels the resting order as the session closes)",
            bne(at)
        ),
        None => "fill: NEVER FILLED (pending order untriggered in window)".to_string(),
    }
}

fn describe_outcome(outcome: &SimOutcome) -> String {
    match outcome {
        SimOutcome::NeverFilled => {
            "fill: NEVER FILLED (pending order untriggered in window)".into()
        }
        SimOutcome::FilledOpen {
            fill_at,
            entry_price,
        } => format!(
            "fill: FILLED @ {entry_price} ({}) — still open at window end",
            bne(*fill_at)
        ),
        SimOutcome::StoppedOut {
            fill_at,
            entry_price,
            exit_at,
            exit_price,
        } => {
            // When the stop landed at the entry price, break-even management
            // (BUG-replay-no-breakeven-stop-at-50pct) moved it there after a
            // candle closed past 50%-to-TP — a 0R scratch, not a −1R loss.
            let label = if (exit_price - entry_price).abs() < 1e-9 {
                "BREAK-EVEN (SL→BE) — in @"
            } else {
                "STOPPED OUT — in @"
            };
            format!(
                "fill: {label} {entry_price} ({}) → SL {exit_price} ({})",
                bne(*fill_at),
                bne(*exit_at)
            )
        }
        SimOutcome::TookProfit {
            fill_at,
            entry_price,
            exit_at,
            exit_price,
        } => format!(
            "fill: TOOK PROFIT — in @ {entry_price} ({}) → TP {exit_price} ({})",
            bne(*fill_at),
            bne(*exit_at)
        ),
        SimOutcome::Unresolved(reason) => format!("fill: UNRESOLVED — {reason}"),
        SimOutcome::Declined { name } => {
            format!("fill: DECLINED — entry past the {name} level (no order placed)")
        }
        SimOutcome::SpreadBlackout {
            spread_pips,
            threshold_pips,
        } => format!(
            "spread: REJECTED — spread {spread_pips:.1}p > {threshold_pips:.1}p threshold inside the NY-close-edge window (no order placed; live worker 423s)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::replay::EnterGateOutcome;
    use super::*;
    use chrono::{TimeZone, Utc};
    use trade_control_core::intent::RiskBudget;

    fn resolved(direction: Direction, entry: ResolvedEntry, sl: f64, tp: f64) -> Resolved {
        Resolved {
            id: "r".into(),
            not_after: Utc.with_ymd_and_hms(2026, 6, 30, 0, 0, 0).unwrap(),
            instrument: "NZD_CHF".into(),
            direction,
            entry,
            stop_loss: sl,
            take_profit: tp,
            risk: RiskBudget::Percent(1.0),
            min_r: 1.0,
            dry_run: false,
            recover_entry: None,
            breakeven: None,
        }
    }

    #[test]
    fn taken_kinds_are_filled_not_taken_kinds_are_not() {
        assert!(FillKind::Open.is_taken());
        assert!(FillKind::StoppedOut.is_taken());
        assert!(FillKind::TookProfit.is_taken());
        assert!(FillKind::ClosedOnReversal.is_taken());
        assert!(!FillKind::NeverFilled.is_taken());
        assert!(!FillKind::Declined.is_taken());
        assert!(!FillKind::SpreadBlackout.is_taken());
        // BUG-replay-golden-gate-not-enforced: a gate-blocked enter is not
        // taken, so it contributes 0R to the entry-style net-R comparison.
        assert!(!FillKind::GateBlocked.is_taken());
    }

    /// A bid==ask==mid bar (zero spread) for the annotation-path tests.
    fn ba(epoch: i64, c: f64) -> trade_control_engine::BidAskCandle {
        let (o, h, l) = (c, c + 0.01, c - 0.01);
        trade_control_engine::BidAskCandle {
            time: at(epoch),
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

    /// A long stop enter that would fill on the forward path — `needs_golden`
    /// toggled by the caller. Built from YAML so the giant `Intent` literal
    /// stays out of the test.
    fn golden_stop_enter(needs_golden: bool) -> trade_control_core::intent::Intent {
        let yaml = format!(
            "
            v: 1
            id: golden-test
            trade_id: golden-test
            not_after: \"2026-06-30T00:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: {{ type: stop, from: high, at: 1.1000 }}
            stop_loss: {{ absolute: 1.0950 }}
            take_profit: {{ absolute: 1.1100 }}
            risk_pct: 1.0
            needs_golden: {needs_golden}
        "
        );
        serde_yaml::from_str(&yaml).expect("parse golden enter intent")
    }

    fn enter_fire(
        intent: trade_control_core::intent::Intent,
        fire_secs: i64,
        gate_outcome: EnterGateOutcome,
    ) -> Fire {
        Fire {
            fired: trade_control_engine::FiredIntent {
                rule_id: "05-enter".into(),
                intent,
                candle: ba(fire_secs, 1.0980).mid(),
                // A stop/limit enter has no latched Pine signal — exactly the
                // case that strips `golden` off the reconstructed shell.
                signal: None,
            },
            // Forward path rises through the 1.1000 stop trigger, then on to the
            // 1.1100 TP — so a `Placed` enter would fill + take profit.
            forward: vec![
                ba(fire_secs, 1.0980),
                ba(fire_secs + 3600, 1.1010),
                ba(fire_secs + 7200, 1.1110),
            ],
            gate_outcome,
            superseded: false,
        }
    }

    fn plan_for(pip_size: f64) -> TradePlan {
        TradePlan {
            trade_id: "golden-test".into(),
            instrument: "EUR_USD".into(),
            direction: Direction::Long,
            granularity: trade_control_engine::Granularity::H1,
            pip_size,
            rules: Vec::new(),
            shadow: false,
        }
    }

    #[test]
    fn annotation_path_blocks_a_rejected_enter() {
        // The entry decision is made by `run_enter` in the tick loop; the report
        // only maps its verdict. A `Rejected` enter (e.g. the live worker 412'd a
        // `needs_golden` enter with golden:None) must render as GateBlocked — not
        // taken, 0R — even though the forward path would otherwise fill to TP.
        let fire = enter_fire(
            golden_stop_enter(true),
            1_781_244_000,
            EnterGateOutcome::Rejected {
                reason: "rejected: needs-golden".into(),
            },
        );
        let result = resolve_fire_any(&plan_for(0.0001), &fire, &[]).expect("an enter result");
        assert_eq!(
            result.kind,
            FillKind::GateBlocked,
            "a run_enter-rejected enter must be gate-blocked, not filled"
        );
        assert!(!result.kind.is_taken());
        // And resolve_fire (taken-only) drops it entirely.
        assert!(resolve_fire(&plan_for(0.0001), &fire, &[]).is_none());
    }

    #[test]
    fn annotation_path_fills_a_placed_enter() {
        // Same enter + path but `run_enter` placed it → the forward path fills to
        // TP, proving the block above is the verdict, not the price path.
        let fire = enter_fire(
            golden_stop_enter(false),
            1_781_244_000,
            EnterGateOutcome::Placed { order_id: None },
        );
        let result = resolve_fire_any(&plan_for(0.0001), &fire, &[]).expect("an enter result");
        assert!(
            result.kind.is_taken(),
            "a placed enter fills along the forward path (got {:?})",
            result.kind
        );
    }

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn cf(secs: i64, price: f64) -> CloseFire {
        CloseFire {
            at: at(secs),
            price,
        }
    }

    #[test]
    fn reversal_close_flattens_an_open_position() {
        // FilledOpen + a reversal after the fill → ClosedOnReversal at that bar.
        let open = SimOutcome::FilledOpen {
            fill_at: at(100),
            entry_price: 5.871,
        };
        match apply_reversal_close(open, &[cf(300, 5.860)]) {
            ReplayOutcome::ClosedOnReversal {
                fill_at,
                entry_price,
                exit_at,
                exit_price,
            } => {
                assert_eq!(fill_at, at(100));
                assert!((entry_price - 5.871).abs() < 1e-9);
                assert_eq!(exit_at, at(300));
                assert!((exit_price - 5.860).abs() < 1e-9);
            }
            other => panic!("expected ClosedOnReversal, got {other:?}"),
        }
    }

    #[test]
    fn reversal_before_the_fill_is_ignored() {
        // A close fire BEFORE the fill belongs to no open position → untouched.
        let open = SimOutcome::FilledOpen {
            fill_at: at(200),
            entry_price: 5.871,
        };
        assert!(matches!(
            apply_reversal_close(open, &[cf(100, 5.860)]),
            ReplayOutcome::Sim(SimOutcome::FilledOpen { .. })
        ));
    }

    #[test]
    fn reversal_after_sl_does_not_override_the_stop() {
        // The position already stopped out at bar 250; a reversal at 300 is moot.
        let stopped = SimOutcome::StoppedOut {
            fill_at: at(100),
            entry_price: 5.871,
            exit_at: at(250),
            exit_price: 5.90,
        };
        assert!(matches!(
            apply_reversal_close(stopped, &[cf(300, 5.86)]),
            ReplayOutcome::Sim(SimOutcome::StoppedOut { .. })
        ));
    }

    #[test]
    fn reversal_before_sl_pre_empts_the_stop() {
        // A reversal that fires strictly before the SL bar closes the position
        // first — the trade exited on the reversal, never reaching its stop.
        let stopped = SimOutcome::StoppedOut {
            fill_at: at(100),
            entry_price: 5.871,
            exit_at: at(400),
            exit_price: 5.90,
        };
        match apply_reversal_close(stopped, &[cf(300, 5.86)]) {
            ReplayOutcome::ClosedOnReversal { exit_at, .. } => assert_eq!(exit_at, at(300)),
            other => panic!("expected ClosedOnReversal, got {other:?}"),
        }
    }

    #[test]
    fn earliest_reversal_wins() {
        let open = SimOutcome::FilledOpen {
            fill_at: at(100),
            entry_price: 5.871,
        };
        match apply_reversal_close(open, &[cf(400, 5.85), cf(250, 5.86)]) {
            ReplayOutcome::ClosedOnReversal { exit_at, .. } => assert_eq!(exit_at, at(250)),
            other => panic!("expected the earliest reversal, got {other:?}"),
        }
    }

    #[test]
    fn never_filled_is_untouched_by_a_reversal() {
        assert!(matches!(
            apply_reversal_close(SimOutcome::NeverFilled, &[cf(300, 5.86)]),
            ReplayOutcome::Sim(SimOutcome::NeverFilled)
        ));
    }

    #[test]
    fn short_stop_order_shows_direction_entry_and_levels() {
        let r = resolved(
            Direction::Short,
            ResolvedEntry::Stop {
                trigger_price: 0.46311,
            },
            0.46338,
            0.46221,
        );
        assert_eq!(
            describe_order(&r, 0.0001),
            "order: SHORT stop @ 0.46311  SL 0.46338  TP 0.46221"
        );
    }

    #[test]
    fn levels_are_rounded_to_the_price_grid_no_float_noise() {
        // The raw resolved SL on the NZD/CHF re-entry was 0.46366999999999997;
        // at pip_size 0.0001 (5 shown decimals) it must read 0.46367.
        let r = resolved(
            Direction::Short,
            ResolvedEntry::Stop {
                trigger_price: 0.46322,
            },
            0.46366999999999997,
            0.46171,
        );
        assert_eq!(
            describe_order(&r, 0.0001),
            "order: SHORT stop @ 0.46322  SL 0.46367  TP 0.46171"
        );
    }

    #[test]
    fn index_pip_size_shows_one_decimal() {
        // A whole-point index grid (pip_size 1.0) shows one fractional digit.
        let r = resolved(
            Direction::Long,
            ResolvedEntry::Stop {
                trigger_price: 6850.0,
            },
            6800.0,
            6950.0,
        );
        assert_eq!(
            describe_order(&r, 1.0),
            "order: LONG stop @ 6850.0  SL 6800.0  TP 6950.0"
        );
    }

    #[test]
    fn long_market_and_limit_entries_label_their_type() {
        let market = resolved(
            Direction::Long,
            ResolvedEntry::Market {
                reference_price: 1.105,
            },
            1.1,
            1.115,
        );
        assert_eq!(
            describe_order(&market, 0.0001),
            "order: LONG market @ 1.10500  SL 1.10000  TP 1.11500"
        );

        let limit = resolved(
            Direction::Long,
            ResolvedEntry::Limit {
                trigger_price: 1.104,
            },
            1.1,
            1.115,
        );
        assert_eq!(
            describe_order(&limit, 0.0001),
            "order: LONG limit @ 1.10400  SL 1.10000  TP 1.11500"
        );
    }
}
