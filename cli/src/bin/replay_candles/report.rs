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
    Action, Direction, Intent, NoEntryWindow, Resolved, ResolvedEntry, Shell,
};
use trade_control_core::plan_sentiment::PlanSentiment;
use trade_control_core::spread_blackout::elevated_threshold_pips;
use trade_control_engine::{
    BidAskCandle as EngineCandle, EntryFloor, SweepReason, TradePlan, apply_entry_spread_floor,
    breakeven_armed_at, sweep_reason, widened_stop_at,
};

use super::brisbane::bne;
use super::replay::{Fire, Replay};
use trade_control_cli::replay_args::DetectorMarkConfig;

/// The tick size the replay report rounds order prices to — the baked
/// `Intent::tick_size` when present, else the plan's `pip_size`. Mirrors the
/// worker's fallback chain (`dispatch::enter`) so the report's resolved prices
/// match what the worker would place. See `simulator::replay_tick`.
fn replay_report_tick(intent: &Intent, plan: &TradePlan) -> f64 {
    intent.tick_size.unwrap_or(plan.pip_size)
}

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
/// Collect the gate-passing `Action::Close` reversal fires over a fire slice —
/// so the replay loop can build the reversal-close set (to hand to
/// `broker.realized_outcome`) before the `Replay` value is assembled.
pub fn collect_close_fires_from(fires: &[Fire]) -> Vec<CloseFire> {
    fires
        .iter()
        .filter(|f| f.fired.intent.action == Action::Close)
        .filter(|f| close_gate_passes(f))
        .map(|f| CloseFire {
            at: f.fired.candle.time,
            price: f.fired.candle.c,
        })
        .collect()
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
    /// An entry the worker's pre-broker gate would have rejected — the
    /// `allow_entry` script returned false/errored, the candle-quality
    /// requirement (`needs_golden` / `needs_confirmed`) the signal-folded shell
    /// didn't meet, or a market-hours / spread-blackout gate rejected the entry.
    /// Not taken (the live worker 4xx's before placing the order). The specific
    /// reason string is carried on the fire's `rejected_reason` and rendered by
    /// the `rejected_reason` branch in `entry_events`.
    GateBlocked,
}

impl FillKind {
    /// Was this position actually taken (an order filled)? `false` for the
    /// not-taken kinds (`NeverFilled` / `Declined` / `GateBlocked`), which
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
    /// The actual exit price for a closed trade (SL / break-even / TP / reversal
    /// bar). `None` for a still-open or not-taken outcome. Sourced from the
    /// broker ledger's `RealizedOutcome::exit_price`, so the report's R and exit
    /// line come from the ledger, not a re-simulation.
    pub exit_price: Option<f64>,
    pub kind: FillKind,
}

/// The taken/filled outcome for one fire, returning a [`FireResult`]
/// **only** when it's an enter that actually filled
/// (`Open` / `StoppedOut` / `TookProfit` / `ClosedOnReversal`). Non-enters,
/// unresolved brackets, declined entries, and never-filled pending orders yield
/// `None` — they have no *taken* position to annotate.
///
/// The fill/exit outcome is the `ReplayBroker` ledger's (PR 4b-2), stashed on
/// the fire by the replay loop; this function reads it, it does not re-simulate.
pub fn resolve_fire(plan: &TradePlan, fire: &Fire) -> Option<FireResult> {
    resolve_fire_any(plan, fire).filter(|r| r.kind.is_taken())
}

/// Like [`resolve_fire`], but also returns the *not-taken* enters —
/// pending orders that never filled (`NeverFilled`) and entries the worker
/// declined (`Declined`). These have no fill bar, so the box is anchored at
/// the **fire bar** (where the order would have been placed) and runs to the
/// window end; `entry_price` is the intended placed level, not a fill.
///
/// Still `None` for non-enters and for enters whose bracket can't resolve
/// (an `Unresolved` outcome — nothing meaningful to draw).
pub fn resolve_fire_any(plan: &TradePlan, fire: &Fire) -> Option<FireResult> {
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

    // The entry decision was made ONCE in the tick loop by the REAL `run_enter`
    // (every gate: pause / retry / cooldown / prep / veto / entry-level-veto /
    // allow_entry / blackouts / SL-floor). A `Rejected` enter is not-taken — the
    // live worker 412/422/423s it before placing an order — so anchor the
    // intended bracket at the fire bar as a `GateBlocked` and tally 0R. This is a
    // gate verdict, not fill physics, so it stays in the report: resolve the raw
    // bracket for the drawn levels and short-circuit before the broker outcome.
    if fire.rejected_reason().is_some() {
        let shell = match &fire.fired.signal {
            Some(sig) => Shell::from_candle_and_signal(candle, sig),
            None => Shell::from_candle(candle),
        };
        let resolved = Resolved::from_intent(
            intent,
            &shell,
            plan.pip_size,
            replay_report_tick(intent, plan),
        )
        .ok()?;
        let window_end = fire.forward.last().map(|c| c.time)?;
        tracing::debug!(
            bar = %candle.time,
            reason = ?fire.rejected_reason(),
            "entry rejected by run_enter — annotation path, not taken (0R)"
        );
        return Some(FireResult {
            direction: resolved.direction,
            fill_at: candle.time,
            until: window_end,
            entry_price: resolved.entry.reference_price(),
            stop_loss: resolved.stop_loss,
            take_profit: resolved.take_profit,
            exit_price: None,
            kind: FillKind::GateBlocked,
        });
    }

    // Fill physics now belong to the broker (PR 4b-2). The replay loop attached
    // this order's geometry to the `ReplayBroker` ledger and stashed the ledger's
    // realized outcome on the fire — fill / exit / floored SL/TP / reversal-close,
    // computed by the SAME engine sims this function used to call directly. The
    // report is pure formatting now: read the broker's verdict and map it to a
    // `FireResult`. `None` means the order was cancelled or its bracket didn't
    // resolve — nothing to draw, exactly as before.
    let r = fire.realized.as_ref()?;
    Some(FireResult {
        direction: r.direction,
        fill_at: r.fill_at,
        until: r.until,
        entry_price: r.entry_price,
        stop_loss: r.stop_loss,
        take_profit: r.take_profit,
        exit_price: r.exit_price,
        kind: r.kind,
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
    sentiment: Option<&PlanSentiment>,
    mark_cfg: &DetectorMarkConfig,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Plan {} ({}, {:?}) — {} fire(s) over the window\n",
        plan.trade_id,
        plan.instrument,
        plan.granularity,
        replay.fires.len()
    ));

    out.push_str(&render_detector_summary(replay, mark_cfg));
    out.push_str(&render_entry_declines(replay));

    if let Some(s) = sentiment {
        out.push_str(&render_sentiment(s));
    }

    if verbose {
        out.push_str(&render_trace(replay));
    }

    let mut tally = Tally::new();
    // A monotonic id for each ENTER fire, so the journal can refer to an entry
    // (and its later widen/restore/break-even events) by a stable "#N" label
    // instead of leaving those events ambiguous when several enters fire.
    let mut entry_no = 0u32;
    // Every event from every fire, collected flat then sorted by time — one
    // top-level chronological stream (the place/fill/widen/restore/exit of all
    // entries interleaved by when they actually happened). The tally is booked
    // in fire order inside `render_fire` (so the compounding sequence is
    // correct); the events only carry the resulting text.
    let mut events: Vec<EntryEvent> = Vec::new();
    for fire in &replay.fires {
        let this_entry = if fire.fired.intent.action == Action::Enter {
            entry_no += 1;
            Some(entry_no)
        } else {
            None
        };
        events.extend(render_fire(
            plan,
            fire,
            this_entry,
            simulate,
            blackout_windows,
            &mut tally,
        ));
    }

    // Stable sort by time: same-bar events keep their emission order (placed
    // before fill before widen on a shared bar).
    events.sort_by_key(|e| e.at);
    for e in &events {
        out.push_str(&format!("{}  {}", bne(e.at), e.note));
        if let Some(close) = e.close {
            out.push_str(&format!("  (close={close})"));
        }
        out.push('\n');
    }

    // Trendline-anchor warnings are intentionally *not* rendered here — they
    // recompute every tick against a growing window (one distinct string per
    // bar) and are low-signal for a normal replay. They're emitted at debug
    // level in `replay.rs` (RUST_LOG=debug) and recorded on the fixture.

    out.push_str(&format!(
        "\nDone: {}  |  final phase: {:?}  |  fires: {}",
        replay.done,
        replay.final_state.phase,
        replay.fires.len()
    ));
    if simulate {
        out.push_str(&format!("  |  TP: {}  SL: {}", tally.wins, tally.losses));
        if tally.reversal_closes > 0 {
            out.push_str(&format!("  REV: {}", tally.reversal_closes));
        }
        out.push_str(&tally.summary_line());
    }
    out.push('\n');
    out
}

/// Render the news-sentiment block: the overall verdict line, then one line per
/// currency with its net score and the events that moved it. Mirrors what
/// `tv-news` logs, formatted for the replay report.
fn render_sentiment(s: &PlanSentiment) -> String {
    let mut out = format!(
        "News sentiment ({} → {}): {}, confidence {}\n",
        bne(s.period_start),
        bne(s.period_end),
        s.overall_direction,
        s.confidence,
    );
    if s.currencies.is_empty() {
        out.push_str("  (no released events in the lookback window)\n");
        return out;
    }
    for c in &s.currencies {
        out.push_str(&format!(
            "  {} — {} (net {:+.1})\n",
            c.currency, c.direction, c.net_score
        ));
        for ev in &c.events {
            out.push_str(&format!("      {ev}\n"));
        }
    }
    out
}

/// The account size a `--simulate` P&L projection compounds from: 1% risk per
/// taken trade against a fresh $100k. Every fill's R multiple grows or shrinks
/// this balance so the report shows what the sequence would have made on a
/// standard account, not just the raw R sum.
const START_ACCOUNT: f64 = 100_000.0;

/// Fraction of the *remaining* account risked on each taken trade (1%).
const RISK_FRACTION: f64 = 0.01;

/// Running tally across a simulated replay: outcome counts, the net R (sum of
/// per-trade R multiples), and a $100k account compounding at 1% risk per
/// taken trade. `net_r` and `account` only move on *taken* fills (TP / SL /
/// reversal-close); not-taken kinds (never-filled, declined, gate-blocked)
/// contribute 0R and leave the balance untouched.
struct Tally {
    wins: usize,
    losses: usize,
    reversal_closes: usize,
    net_r: f64,
    account: f64,
}

impl Tally {
    fn new() -> Self {
        Self {
            wins: 0,
            losses: 0,
            reversal_closes: 0,
            net_r: 0.0,
            account: START_ACCOUNT,
        }
    }

    /// Book one taken trade's R multiple: add it to the net R and compound the
    /// account by `1% × account × R` (the P&L of risking 1% of what's left).
    /// Returns the dollar P&L of this trade so the per-fill line can show it.
    fn book(&mut self, r: f64) -> f64 {
        self.net_r += r;
        let pnl = RISK_FRACTION * self.account * r;
        self.account += pnl;
        pnl
    }

    /// The trailing summary segment: net R and the compounded $100k-account P&L.
    fn summary_line(&self) -> String {
        let profit = self.account - START_ACCOUNT;
        format!(
            "  |  Net R: {:+.2}  |  $100k acct (1%/trade): ${:.0} ({:+.0})",
            self.net_r, self.account, profit
        )
    }
}

/// The realized R multiple of a taken fill: signed reward over risk. `entry −
/// stop_loss` is the risk (positive for a long, negative for a short), and
/// `exit − entry` is the reward with the trade's own sign, so the quotient is
/// `+1` on a clean TP and `−1` on a clean SL for *both* directions without a
/// direction branch. Returns `0.0` when the stop sits at the entry (a
/// degenerate/zero-risk bracket) so it can't divide by zero.
fn realized_r(entry: f64, stop_loss: f64, exit: f64) -> f64 {
    let risk = entry - stop_loss;
    if risk.abs() < f64::EPSILON {
        return 0.0;
    }
    (exit - entry) / risk
}

/// The always-on candle-detector summary line: how many bars the detector
/// printed a signal the active `--candle-detector-*` filter accepted, split by
/// golden / non-golden. This is the "golden candle we never entered on" count —
/// it's driven off the SAME per-bar marks the `--verbose` trace renders in
/// detail. Returns an empty string when marking is off (either axis `none`), per
/// the "omit summary when off" decision.
fn render_detector_summary(replay: &Replay, mark_cfg: &DetectorMarkConfig) -> String {
    if mark_cfg.is_off() {
        return String::new();
    }
    let marks = replay.traces.iter().filter_map(|t| t.detected.as_ref());
    let (mut golden, mut non_golden) = (0usize, 0usize);
    for m in marks {
        if m.golden {
            golden += 1;
        } else {
            non_golden += 1;
        }
    }
    let total = golden + non_golden;
    // Describe the active filter so the count is unambiguous (e.g. a `0` under
    // `with golden` vs a `0` under `both both` mean different things).
    format!(
        "Candle detector ({:?} / {:?}): {total} bar(s) marked — {golden} golden, {non_golden} non-golden\n",
        mark_cfg.direction, mark_cfg.golden
    )
}

/// Always-on entry-decline rollup: bars on which a `PinePattern` enter's signal
/// fired + matched direction but the engine's pre-flight declined it
/// (needs-golden / needs-confirmed / resolve-failed like below-min-R). This is
/// the direct answer to "the golden printed but nothing entered — why?"; it's
/// **independent** of the `--candle-detector-*` marking (a decline is worth
/// showing whether or not the operator asked for golden marks) and always on.
/// Empty string when no enter declined over the window.
fn render_entry_declines(replay: &Replay) -> String {
    let declined: Vec<(chrono::DateTime<Utc>, &String)> = replay
        .traces
        .iter()
        .flat_map(|t| t.entry_declines.iter().map(move |r| (t.bar, r)))
        .collect();
    if declined.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "Entry declines: {} bar(s) an enter fired but was skipped —\n",
        declined.len()
    );
    for (bar, reason) in declined {
        out.push_str(&format!("  {}  ✗ {reason}\n", bne(bar)));
    }
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

/// Build the flat, time-ordered event stream for one fire. Every event is a
/// top-level line (`render` sorts them across all fires by time); nothing is
/// nested. Books the tally in fire order (correct compounding) and folds the
/// resulting R into the exit event's note. Non-enter fires (prep / close /
/// other) contribute their own single event.
fn render_fire(
    plan: &TradePlan,
    fire: &Fire,
    entry_no: Option<u32>,
    simulate: bool,
    blackout_windows: &[NoEntryWindow],
    tally: &mut Tally,
) -> Vec<EntryEvent> {
    let intent = &fire.fired.intent;
    let candle = &fire.fired.candle;
    // Stable "entry #N" label so each event names which entry it belongs to in
    // the merged stream (several enters can be in flight at once).
    let ev = match entry_no {
        Some(n) => format!("entry #{n}"),
        None => "entry".to_string(),
    };

    // Without --simulate we only note the fire itself (no fill sim / bracket).
    if !simulate {
        return vec![EntryEvent {
            at: candle.time,
            note: format!("{} {:?} ({ev})", fire.fired.rule_id, intent.action),
            close: Some(candle.c),
        }];
    }
    if intent.action == Action::Prep {
        // A prep carries no bracket of its own — preview the plan's *enter*
        // bracket resolved against this prep bar so the journal shows the SL/TP
        // the eventual order would carry even when it never fills.
        return vec![EntryEvent {
            at: candle.time,
            note: format!(
                "prep ({}) — {}",
                fire.fired.rule_id,
                prep_preview(plan, fire)
            ),
            close: Some(candle.c),
        }];
    }
    if intent.action == Action::Close {
        // A close fire flattens whatever enter is open (or is blocked by the
        // allow_close gate, keeping the position open).
        let note = if close_gate_passes(fire) {
            format!(
                "close-on-reversal ({}) — flattens the open position",
                fire.fired.rule_id
            )
        } else {
            format!(
                "close BLOCKED by allow_close gate ({}) — position stays open",
                fire.fired.rule_id
            )
        };
        return vec![EntryEvent {
            at: candle.time,
            note,
            close: Some(candle.c),
        }];
    }
    if intent.action != Action::Enter {
        return vec![EntryEvent {
            at: candle.time,
            note: format!(
                "{:?} ({}) — no fill simulated",
                intent.action, fire.fired.rule_id
            ),
            close: Some(candle.c),
        }];
    }

    // Reconstruct the shell exactly as the worker's dispatch would, so the
    // simulator resolves the same entry/SL/TP levels.
    let shell = match &fire.fired.signal {
        Some(sig) => Shell::from_candle_and_signal(candle, sig),
        None => Shell::from_candle(candle),
    };

    // The "placed" event carries the resolved bracket (direction, entry, SL,
    // TP) — the journaling record of what order went on. The spread-floor widen
    // is applied so the shown SL is the protected stop, not the signed level.
    // The fill's R is scored off the ledger's floored stop (`resolve_fire_any`
    // below), not this preview — this block only formats the placed line.
    let placed_note = match Resolved::from_intent(
        intent,
        &shell,
        plan.pip_size,
        replay_report_tick(intent, plan),
    ) {
        Ok(mut resolved) => {
            let signed_sl = resolved.stop_loss;
            let floor = apply_entry_spread_floor(
                &mut resolved,
                plan.pip_size,
                &fire.forward,
                fire.entry_spread_price,
            );
            let mut note = format!("{ev} placed — {}", describe_order(&resolved, plan.pip_size));
            // When the floor moved the stop, note the spread that sized it.
            if let EntryFloor::Applied { spread_pips } = floor
                && (resolved.stop_loss - signed_sl).abs() > f64::EPSILON
            {
                note.push_str(&format!(
                    " [SL floored to 10× spread ({spread_pips:.1}p @ entry bar); signed SL was {}]",
                    fmt_price(signed_sl, plan.pip_size),
                ));
            }
            note
        }
        Err(err) => format!("{ev} placed — order UNRESOLVED: {err:?}"),
    };
    let mut events: Vec<EntryEvent> = vec![EntryEvent {
        at: candle.time,
        note: placed_note,
        close: Some(candle.c),
    }];

    // A superseded enter had its resting order cancelled by a later entry
    // (cancel-and-replace) before it could fill — report the cancellation and
    // don't tally it.
    if fire.superseded {
        events.push(EntryEvent {
            at: candle.time,
            note: format!(
                "{ev} SUPERSEDED — resting order cancelled by a later entry (cancel-and-replace)"
            ),
            close: Some(candle.c),
        });
        return events;
    }

    // The entry decision came from the REAL `run_enter`. A `Rejected` enter is a
    // 0R skip — surface the real dispatch reason and don't simulate a fill.
    if let Some(reason) = fire.rejected_reason() {
        let detail = if reason.starts_with("rejected: paused") {
            format!("{ev} SUPPRESSED — trade paused by news blackout [{reason}] → NO FILL / 0R")
        } else {
            format!("{ev} BLOCKED — {reason} → NO FILL / 0R")
        };
        events.push(EntryEvent {
            at: candle.time,
            note: detail,
            close: Some(candle.c),
        });
        return events;
    }

    // Break-even arming: the bar whose close runs past 50%-to-TP. The live cron
    // (`breakeven_watch`) amends the broker SL to entry here.
    if let Some(armed_at) = breakeven_armed_at(
        intent,
        &shell,
        plan.pip_size,
        &fire.forward,
        fire.entry_spread_price,
    ) {
        events.push(EntryEvent {
            at: armed_at,
            note: format!("{ev} SL→break-even (a candle closed past 50%-to-TP)"),
            close: bar_close(&fire.forward, armed_at),
        });
    }

    // Spread-widen (System 2): a spread-hour bar moves the broker SL *away* from
    // price (`blackout_apply`), transiently — the recovery watcher
    // (`blackout_watch`) restores the original once the spread recovers (≤4p) or
    // the 3h backstop fires. We surface BOTH so the journal shows the shield
    // snapping back, not a permanent risk change. `simulate_fill` now ALSO applies
    // this same widen (via the shared `widened_stop_at`) when scoring the exit — so
    // a spread-hour spike that clears the widened stop no longer books a false
    // stop-out. These journal lines and the scored outcome read the same
    // reconstruction, so they can't disagree.
    let widen_trigger = elevated_threshold_pips(&intent.instrument);
    if let Some(widen) = widened_stop_at(
        intent,
        &shell,
        plan.pip_size,
        &fire.forward,
        widen_trigger,
        fire.entry_spread_price,
    ) {
        events.push(EntryEvent {
            at: widen.at,
            note: format!(
                "{ev} SL widened → {} (spread blackout System 2, transient, {:.1}p; from {})",
                fmt_price(widen.widened_stop, plan.pip_size),
                widen.widen_spread_pips,
                fmt_price(widen.original_stop, plan.pip_size),
            ),
            close: bar_close(&fire.forward, widen.at),
        });
        match widen.restored_at {
            Some(restored_at) => events.push(EntryEvent {
                at: restored_at,
                note: format!(
                    "{ev} SL restored → {} (spread recovered / backstop — widen was transient)",
                    fmt_price(widen.original_stop, plan.pip_size),
                ),
                close: bar_close(&fire.forward, restored_at),
            }),
            None => {
                let at = fire.forward.last().map(|c| c.time).unwrap_or(widen.at);
                events.push(EntryEvent {
                    at,
                    note: format!("{ev} SL still widened at window end (spread never recovered)"),
                    close: bar_close(&fire.forward, at),
                });
            }
        }
    }

    // Fill physics belong to the broker ledger (PR 4b-2/4b-4). The single fill
    // verdict is `fire.realized`, read here via `resolve_fire_any` — the SAME
    // source the `--annotate` path uses — so the text journal and the chart can
    // never disagree. This function no longer re-simulates the fill; it formats
    // the ledger's outcome (kind + entry / exit / R). A `None` verdict means the
    // order never went on (cancelled by the spread-hour lifecycle and NOT
    // restored, or an unresolved bracket) — render that as a 0R no-fill, don't
    // tally it. Before this change, a direct `simulate_fill_windowed` here walked
    // the original resting order's path blind to the lifecycle cancel, so a
    // cancelled order still reported a full FILLED → exit → R sequence.
    let Some(result) = resolve_fire_any(plan, fire) else {
        events.push(EntryEvent {
            at: candle.time,
            note: format!("{ev} NO FILL — order cancelled in spread hour (not restored) → 0R"),
            close: Some(candle.c),
        });
        return events;
    };
    let entry_price = result.entry_price;
    // Score R off the ledger's floored stop + actual exit — the ledger is the
    // single source of truth for the fill.
    let ledger_stop = Some(result.stop_loss);
    let exit_price = result.exit_price.unwrap_or(entry_price);
    match result.kind {
        FillKind::ClosedOnReversal => {
            events.push(EntryEvent {
                at: result.fill_at,
                note: format!("{ev} FILLED @ {}", fmt_price(entry_price, plan.pip_size)),
                close: bar_close(&fire.forward, result.fill_at),
            });
            tally.reversal_closes += 1;
            let r = book_r(tally, ledger_stop, entry_price, exit_price);
            events.push(EntryEvent {
                at: result.until,
                note: format!(
                    "{ev} CLOSED ON REVERSAL → {}{r}",
                    fmt_price(exit_price, plan.pip_size)
                ),
                close: bar_close(&fire.forward, result.until),
            });
        }
        FillKind::Open => events.push(EntryEvent {
            at: result.fill_at,
            note: format!(
                "{ev} FILLED @ {} (still open at window end)",
                fmt_price(entry_price, plan.pip_size)
            ),
            close: bar_close(&fire.forward, result.fill_at),
        }),
        FillKind::TookProfit => {
            events.push(EntryEvent {
                at: result.fill_at,
                note: format!("{ev} FILLED @ {}", fmt_price(entry_price, plan.pip_size)),
                close: bar_close(&fire.forward, result.fill_at),
            });
            tally.wins += 1;
            let r = book_r(tally, ledger_stop, entry_price, exit_price);
            events.push(EntryEvent {
                at: result.until,
                note: format!(
                    "{ev} TOOK PROFIT → {}{r}",
                    fmt_price(exit_price, plan.pip_size)
                ),
                close: bar_close(&fire.forward, result.until),
            });
        }
        FillKind::StoppedOut => {
            events.push(EntryEvent {
                at: result.fill_at,
                note: format!("{ev} FILLED @ {}", fmt_price(entry_price, plan.pip_size)),
                close: bar_close(&fire.forward, result.fill_at),
            });
            tally.losses += 1;
            let r = book_r(tally, ledger_stop, entry_price, exit_price);
            // A break-even scratch (SL→BE moved the stop to entry) exits at the
            // entry price for 0R, not a −1R loss — label it as such.
            let label = if (exit_price - entry_price).abs() < 1e-9 {
                "SL→BREAK-EVEN"
            } else {
                "STOPPED OUT"
            };
            events.push(EntryEvent {
                at: result.until,
                note: format!("{ev} {label} → {}{r}", fmt_price(exit_price, plan.pip_size)),
                close: bar_close(&fire.forward, result.until),
            });
        }
        // Not-taken outcomes the ledger owns: a single note on the fire bar, no
        // tally move. `NeverFilled` carries a sweep reason; the others describe
        // the gate/quality decline.
        FillKind::NeverFilled => {
            let swept = sweep_reason(
                intent,
                &shell,
                plan.pip_size,
                &fire.forward,
                blackout_windows,
            );
            events.push(EntryEvent {
                at: candle.time,
                note: format!("{ev} {}", describe_never_filled(swept)),
                close: Some(candle.c),
            });
        }
        FillKind::Declined => events.push(EntryEvent {
            at: candle.time,
            note: format!("{ev} fill: DECLINED — entry past a gate level (no order placed)"),
            close: Some(candle.c),
        }),
        // A gate rejection is handled by the `rejected_reason` branch above; if
        // the ledger ever surfaces it here, render it plainly.
        FillKind::GateBlocked => events.push(EntryEvent {
            at: candle.time,
            note: format!("{ev} BLOCKED by a pre-broker gate → NO FILL / 0R"),
            close: Some(candle.c),
        }),
    }

    events
}

/// The close of the forward-path bar at `at`, if present — for the `close=…`
/// context on an event that lands on a later bar (widen / restore / exit).
fn bar_close(forward: &[EngineCandle], at: DateTime<Utc>) -> Option<f64> {
    forward.iter().find(|c| c.time == at).map(|c| c.c)
}

/// Score a taken fill's R multiple against its protected stop, book it into the
/// running tally (net R + compounding $100k account), and return the trailing
/// `  (R: …  $100k acct: …)` fragment to append to the exit event. A `None`
/// stop (bracket that never resolved) books nothing and returns an empty
/// string.
fn book_r(
    tally: &mut Tally,
    protected_stop: Option<f64>,
    entry_price: f64,
    exit_price: f64,
) -> String {
    let Some(stop_loss) = protected_stop else {
        return String::new();
    };
    let r = realized_r(entry_price, stop_loss, exit_price);
    let pnl = tally.book(r);
    format!(
        "  (R: {r:+.2}  |  $100k acct (1% risk): {:+.0} → ${:.0})",
        pnl, tally.account
    )
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
fn prep_preview(plan: &TradePlan, fire: &Fire) -> String {
    let Some(enter) = plan_enter_intent(plan) else {
        return "plan has no enter rule to preview".to_string();
    };
    let candle = &fire.fired.candle;
    let shell = match &fire.fired.signal {
        Some(sig) => Shell::from_candle_and_signal(candle, sig),
        None => Shell::from_candle(candle),
    };
    match Resolved::from_intent(
        enter,
        &shell,
        plan.pip_size,
        replay_report_tick(enter, plan),
    ) {
        Ok(resolved) => format!(
            "would-enter: {}",
            describe_order(&resolved, plan.pip_size)
                .strip_prefix("order: ")
                .unwrap_or("?")
        ),
        Err(_) => "enter bracket not resolvable at this bar yet".to_string(),
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

/// One top-level event in the flat, time-ordered replay stream — the bar's
/// time, the note (e.g. "entry #1 placed — LONG limit …", "entry #1 FILLED"),
/// and optionally the bar's close for price context. `render` sorts these by
/// `at` across all fires and prints one per line.
struct EntryEvent {
    at: DateTime<Utc>,
    note: String,
    close: Option<f64>,
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

#[cfg(test)]
mod tests {
    use super::super::replay::EnterGateOutcome;
    use super::*;
    use chrono::{TimeZone, Utc};
    use trade_control_cli::replay_args::{DirectionFilter, GoldenFilter};

    /// Detector marking off — the summary line is empty, so report assertions
    /// that predate this feature stay byte-stable.
    fn no_marks() -> DetectorMarkConfig {
        DetectorMarkConfig::new(DirectionFilter::None, GoldenFilter::None, Direction::Long)
    }
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
    fn realized_r_is_plus_one_on_a_clean_tp_both_directions() {
        // Long: entry 1.10, SL 1.09 (risk 0.01), TP 1.11 → +1R.
        assert!((realized_r(1.10, 1.09, 1.11) - 1.0).abs() < 1e-9);
        // Short: entry 1.10, SL 1.11 (risk 0.01 the other way), TP 1.09 → +1R.
        assert!((realized_r(1.10, 1.11, 1.09) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn realized_r_is_minus_one_on_a_clean_sl_both_directions() {
        // Long stopped at its SL, short stopped at its SL → −1R each.
        assert!((realized_r(1.10, 1.09, 1.09) + 1.0).abs() < 1e-9);
        assert!((realized_r(1.10, 1.11, 1.11) + 1.0).abs() < 1e-9);
    }

    #[test]
    fn realized_r_scales_with_a_partial_move() {
        // Long risk 0.01, exit +0.005 → +0.5R; break-even scratch → 0R.
        assert!((realized_r(1.10, 1.09, 1.105) - 0.5).abs() < 1e-9);
        assert!(realized_r(1.10, 1.09, 1.10).abs() < 1e-9);
    }

    #[test]
    fn realized_r_zero_risk_bracket_is_zero_not_nan() {
        // SL sitting at the entry is a degenerate/zero-risk bracket — 0R, no div0.
        let r = realized_r(1.10, 1.10, 1.11);
        assert_eq!(r, 0.0);
    }

    #[test]
    fn tally_compounds_a_100k_account_at_one_percent_risk() {
        let mut t = Tally::new();
        assert_eq!(t.account, 100_000.0);
        // A +1R win risks 1% of 100k → +$1,000, account 101,000.
        let pnl = t.book(1.0);
        assert!((pnl - 1_000.0).abs() < 1e-6);
        assert!((t.account - 101_000.0).abs() < 1e-6);
        // A −1R loss now risks 1% of 101k → −$1,010, account 99,990.
        let pnl = t.book(-1.0);
        assert!((pnl + 1_010.0).abs() < 1e-6);
        assert!((t.account - 99_990.0).abs() < 1e-6);
        // Net R is the plain sum of the two multiples.
        assert!(t.net_r.abs() < 1e-9);
    }

    #[test]
    fn tally_summary_line_shows_net_r_and_projected_profit() {
        let mut t = Tally::new();
        t.book(1.0); // +$1,000 → 101,000
        t.book(1.0); // +1% of 101,000 = +$1,010 → 102,010
        let s = t.summary_line();
        assert!(s.contains("Net R: +2.00"), "got: {s}");
        assert!(s.contains("$102010"), "got: {s}");
        assert!(s.contains("+2010"), "got: {s}");
    }

    #[test]
    fn book_r_is_a_noop_without_a_stop() {
        // An unresolved bracket (no protected stop) can't be scored → tally
        // untouched, empty fragment returned.
        let mut t = Tally::new();
        let frag = book_r(&mut t, None, 1.10, 1.11);
        assert!(frag.is_empty());
        assert_eq!(t.net_r, 0.0);
        assert_eq!(t.account, 100_000.0);
    }

    #[test]
    fn book_r_scores_and_formats_a_win() {
        let mut t = Tally::new();
        // Long: entry 1.10, SL 1.09, TP 1.11 → +1R, +$1,000.
        let frag = book_r(&mut t, Some(1.09), 1.10, 1.11);
        assert!((t.net_r - 1.0).abs() < 1e-9);
        assert!((t.account - 101_000.0).abs() < 1e-6);
        assert!(frag.contains("R: +1.00"), "got: {frag}");
        assert!(frag.contains("$101000"), "got: {frag}");
    }

    #[test]
    fn taken_kinds_are_filled_not_taken_kinds_are_not() {
        assert!(FillKind::Open.is_taken());
        assert!(FillKind::StoppedOut.is_taken());
        assert!(FillKind::TookProfit.is_taken());
        assert!(FillKind::ClosedOnReversal.is_taken());
        assert!(!FillKind::NeverFilled.is_taken());
        assert!(!FillKind::Declined.is_taken());
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
        // Forward path rises through the 1.1000 stop trigger, then on to the
        // 1.1100 TP — so a `Placed` enter would fill + take profit.
        let forward = vec![
            ba(fire_secs, 1.0980),
            ba(fire_secs + 3600, 1.1010),
            ba(fire_secs + 7200, 1.1110),
        ];
        let fired = trade_control_engine::FiredIntent {
            rule_id: "05-enter".into(),
            intent,
            candle: ba(fire_secs, 1.0980).mid(),
            // A stop/limit enter has no latched Pine signal — exactly the case
            // that strips `golden` off the reconstructed shell.
            signal: None,
        };
        // Mirror the replay loop: a placed enter gets its outcome from the broker
        // ledger, which the report then reads (PR 4b-2). A rejected/other gate
        // outcome leaves `realized: None` — the report renders it from gate state.
        let realized = if matches!(gate_outcome, EnterGateOutcome::Placed { .. }) {
            let broker =
                crate::replay_candles::replay_broker::ReplayBroker::new(forward.clone(), 0.0001);
            let shell = Shell::from_candle(&fired.candle);
            broker.record_order(
                "e1".into(),
                fired.intent.clone(),
                shell,
                forward.clone(),
                None,
            );
            broker.realized_outcome("e1", &[])
        } else {
            None
        };
        Fire {
            fired,
            forward,
            gate_outcome,
            superseded: false,
            // No windowed spread in the fixture → the floor falls back to the
            // fire bar's own close spread, the pre-window behaviour these tests
            // were written against.
            entry_spread_price: None,
            realized,
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
            cross_buffer_pct: 0.0,
            cross_buffer_atr: 0.0,
            retest_atr_step: trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            replay_start: None,
            armed_at: None,
            armed_sentiment: None,
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
        let result = resolve_fire_any(&plan_for(0.0001), &fire).expect("an enter result");
        assert_eq!(
            result.kind,
            FillKind::GateBlocked,
            "a run_enter-rejected enter must be gate-blocked, not filled"
        );
        assert!(!result.kind.is_taken());
        // And resolve_fire (taken-only) drops it entirely.
        assert!(resolve_fire(&plan_for(0.0001), &fire).is_none());
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
        let result = resolve_fire_any(&plan_for(0.0001), &fire).expect("an enter result");
        assert!(
            result.kind.is_taken(),
            "a placed enter fills along the forward path (got {:?})",
            result.kind
        );
    }

    #[test]
    fn render_emits_a_flat_time_ordered_event_stream() {
        // A placed enter that fills then takes profit → the report is a flat,
        // top-level, chronological event stream: `placed` on the fire bar,
        // `FILLED` on the trigger bar, `TOOK PROFIT` on the TP bar — each its
        // own line at its own timestamp, none nested under the entry.
        let fire = enter_fire(
            golden_stop_enter(false),
            1_781_244_000,
            EnterGateOutcome::Placed { order_id: None },
        );
        let replay = Replay {
            fires: vec![fire],
            final_state: trade_control_engine::PlanState::seed(
                trade_control_engine::Phase::Done,
                at(1_781_244_000 + 7200),
            ),
            done: true,
            warnings: Vec::new(),
            traces: Vec::new(),
        };
        let out = render(
            &plan_for(0.0001),
            &replay,
            true,
            false,
            &[],
            None,
            &no_marks(),
        );

        // Top-level event lines, each naming entry #1 — no "bars (entry
        // timeline):" sub-heading, no OHLC dump, no leading-indent nested
        // "    order:" block (the bracket is inline on the placed line).
        assert!(
            out.contains("entry #1 placed — order: LONG"),
            "placed line:\n{out}"
        );
        assert!(out.contains("entry #1 FILLED @"), "fill line:\n{out}");
        assert!(
            out.contains("entry #1 STOPPED OUT →") || out.contains("entry #1 TOOK PROFIT →"),
            "exit line:\n{out}"
        );
        assert!(
            !out.contains("bars (entry timeline)"),
            "no candle listing:\n{out}"
        );
        assert!(
            !out.contains("\n    order:"),
            "no nested order line:\n{out}"
        );

        // Events are time-ordered: placed before fill before exit.
        let placed = out.find("placed").expect("placed present");
        let filled = out.find("FILLED").expect("filled present");
        let exit = out
            .find("STOPPED OUT")
            .or_else(|| out.find("TOOK PROFIT"))
            .expect("exit present");
        assert!(
            placed < filled && filled <= exit,
            "not time-ordered:\n{out}"
        );

        // The exit line carries the booked R fragment.
        assert!(out.contains("R: "), "exit carries R:\n{out}");
    }

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    // The reversal-close post-pass now lives on the broker ledger
    // (`replay_broker.rs::apply_reversal_close` + `shadow_parity_fill_then_reversal_close`),
    // the single fill-verdict source the report reads via `resolve_fire_any`; the
    // former report-layer copy + its unit tests were removed in PR 4b-4.

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

    #[test]
    fn renders_sentiment_block() {
        use trade_control_core::plan_sentiment::{CurrencySnapshot, PlanSentiment};
        let snap = PlanSentiment {
            period_start: at(0),
            period_end: at(86_400),
            overall_direction: "bullish".into(),
            confidence: "high".into(),
            currencies: vec![CurrencySnapshot {
                currency: "EUR".into(),
                direction: "bullish".into(),
                net_score: 3.0,
                events: vec!["GDP q/q: Actual (0.8%) beat forecast (0.5%)".into()],
            }],
        };
        let out = render_sentiment(&snap);
        assert!(out.contains("News sentiment"), "header:\n{out}");
        assert!(out.contains("bullish, confidence high"), "verdict:\n{out}");
        assert!(out.contains("EUR — bullish (net +3.0)"), "currency:\n{out}");
        assert!(out.contains("GDP q/q"), "event line:\n{out}");
    }

    #[test]
    fn renders_sentiment_block_with_no_events() {
        use trade_control_core::plan_sentiment::PlanSentiment;
        let snap = PlanSentiment {
            period_start: at(0),
            period_end: at(86_400),
            overall_direction: "neutral".into(),
            confidence: "low".into(),
            currencies: vec![],
        };
        let out = render_sentiment(&snap);
        assert!(out.contains("no released events"), "empty note:\n{out}");
    }
}
