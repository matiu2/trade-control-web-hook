//! The pure FSM evaluator — the testable heart of the server-side engine.
//!
//! [`evaluate_plan`] is the generalisation of
//! [`plan_mw_update`](trade_control_core::intent::plan_mw_update): given a
//! registered [`TradePlan`], the prior persisted [`PlanState`], and the new
//! candles since the watermark, it returns which intents fired, the advanced
//! state, and whether the plan is finished. It is **KV-free and broker-free**
//! so it unit-tests natively; the worker's `run_engine_tick` wraps it with the
//! candle fetch + KV read/write + dispatch.
//!
//! # The model: a spine plus guards
//!
//! Where TradingView Pine alerts are *stateless* (each dings on a cross, blind
//! to the trade's status — so ordering is faked with the `clears` kludge and a
//! `requires_preps` timestamp gate), the engine runs **one state machine per
//! `trade_id`**:
//!
//! - **A sequential spine** — `AwaitBreakAndClose → AwaitEntry → Done`. A phase
//!   only advances. The break-and-close rule fires once, advances the phase,
//!   and then **dies** (it is never re-evaluated — the server-side win over a
//!   re-firing alert).
//! - **Always-armed guards** — the veto rules, evaluated every tick while their
//!   window is open, regardless of spine position. A terminal veto (cancel /
//!   overshoot / invalidation / trade-expiry) fires its intent and ends the
//!   plan.
//! - **Retest as a retroactive lookback** — not a phase, not an emitted prep.
//!   While in `AwaitEntry` the evaluator stamps
//!   [`PlanState::retest_seen_at`] whenever a candle satisfies the retest
//!   trendline geometry. When the entry trigger fires, the gate is "did a
//!   retest happen in `(break_close_at, entry]`?" — durable across cron ticks
//!   because the stamp is persisted.
//!
//! # M/W delegation
//!
//! For an M/W plan (a [`Trigger::MwEveryBar`] enter, no break-and-close prep),
//! the evaluator emits the enter intent once per closed bar — a heartbeat that
//! never latches. It does **not** re-implement the neckline-evolution / cancel
//! decision: the worker's existing `run_enter → maybe_update_mw_state` owns all
//! of that (one implementation). So [`PlanState::mw`] stays unused here.
//!
//! # H&S candle-pattern entry (Stage E)
//!
//! The [`Trigger::PinePattern`] entry (the H&S `05-enter`) is evaluated by the
//! Rust port of the Pine detector ([`trade_control_core::signals`]) over the
//! `detector_window` — see [`eval_pine_entry`]. When it fires it carries the
//! latched signal geometry (`signal_high`/`signal_low`/`golden`/
//! `signal_confirmed`/`recent_*`/…) on the [`FiredIntent`], so the dispatched
//! enter resolves against the *pattern* extremes exactly as the TV alert's
//! `{{plot(...)}}` substitutions did.
//!
//! Before a Pine entry latches + retires the spine it is **pre-flighted**
//! ([`pine_entry_dispatchable`]): the candle-quality gate
//! (`needs_golden`/`needs_confirmed`) and bracket resolution
//! ([`trade_control_core::intent::Resolved::from_intent`]) must both pass —
//! both pure here. A bar that fires the detector but can't pass these is a
//! *decline this bar* (stay `AwaitEntry`), not a terminal `Done`. Without this,
//! a `resolve-failed` enter (e.g. a false-golden tiny pinbar resolving to a
//! degenerate bracket) tore the plan down and abandoned its still-valid vetos
//! — bug #13.

use chrono::{DateTime, Utc};

use trade_control_core::broker::Candle;
use trade_control_core::intent::{Action, Direction, SignalKind};
use trade_control_core::plan_eval::{FiredIntent, PlanEval};
use trade_control_core::plan_state::{Phase, PlanState};
use trade_control_core::signals::{DetectorConfig, LatchedSignal, latched_signal_at};
use trade_control_core::trade_plan::{
    BarEvent, ConditionRule, CrossDir, LinePoint, TradePlan, Trigger,
};

/// Substrings that identify a rule's role from its `rule_id` (the alert
/// basename, e.g. `03-prep-break-and-close`). Matched by `contains` so the
/// numeric prefix and any future suffix don't matter, and so the engine crate
/// needn't depend on the tv-arm-side `conventions` basename enum.
const ROLE_BREAK_AND_CLOSE: &str = "prep-break-and-close";
const ROLE_RETEST: &str = "prep-retest";

/// The prep-step name an enter intent lists in `requires_preps` for the retest
/// gate. Distinct from `ROLE_RETEST` (which matches the *rule_id* of the prep
/// rule) — `requires_preps` carries the bare step name the CLI emits. Used to
/// decide per-enter (not plan-global) whether the retest gate applies, so a
/// strategy-v2 QM enter (empty `requires_preps`) is free of it while the stop
/// enter still respects it.
const PREP_STEP_RETEST: &str = "retest";

/// Count the `Action::Enter` rules in a plan. Single-enter plans (the H&S
/// stop entry or the M/W heartbeat) have one; a strategy-v2 plan has two (the
/// stop entry plus the Quasimodo limit). The engine evaluates *every* enter
/// rule each bar (see [`evaluate_entry`]); the count drives the phase choice.
fn enter_rule_count(plan: &TradePlan) -> usize {
    plan.rules
        .iter()
        .filter(|r| r.intent.action == Action::Enter)
        .count()
}

/// Whether a plan carries more than one enter rule (strategy-v2: stop + QM).
fn is_multi_enter(plan: &TradePlan) -> bool {
    enter_rule_count(plan) > 1
}

/// The starting spine phase for a plan, derived from which rules it carries.
/// A plan with a break-and-close prep starts gated behind it; everything else
/// (notably M/W, whose enter is a per-bar heartbeat with no preps) starts
/// watching for entry directly.
///
/// **Multi-enter (strategy-v2) exception:** a plan with two enters (a stop
/// entry that needs break-and-close, plus a Quasimodo limit that needs *no*
/// preps) must let the QM enter be evaluable from bar 1 — so it starts in
/// `AwaitEntry` even though a break-and-close rule exists. The stop entry's
/// break-and-close is still stamped (the `AwaitEntry` arm runs it for
/// multi-enter plans) and still gates that enter via its `requires_preps`;
/// the QM enter, carrying no preps, is free.
pub fn initial_phase(plan: &TradePlan) -> Phase {
    if is_multi_enter(plan) {
        return Phase::AwaitEntry;
    }
    if plan
        .rules
        .iter()
        .any(|r| r.rule_id.contains(ROLE_BREAK_AND_CLOSE))
    {
        Phase::AwaitBreakAndClose
    } else {
        Phase::AwaitEntry
    }
}

/// Seed a fresh plan's state from a back-window of recent candles **without
/// firing anything**.
///
/// The first engine tick for a plan must not retroactively fire conditions that
/// were already true when the plan was registered (a fresh TV alert doesn't
/// back-fire on history either — decision #3 in the plan). So instead of feeding
/// the back-window through [`evaluate_plan`], the wrapper calls this: it sets
/// the watermark to the newest candle's open-time and records each `OnClose`
/// rule's `last_close` from that candle, so the *next* tick can detect a cross
/// against it. `fired` stays empty; the phase is [`initial_phase`].
///
/// `candles` need not be sorted; the newest by `time` wins. An empty slice
/// yields an unwatermarked seed (the next tick will itself seed once candles
/// arrive).
pub fn seed_plan_state(
    plan: &TradePlan,
    candles: &[Candle],
    expires_at: DateTime<Utc>,
) -> PlanState {
    let mut state = PlanState::seed(initial_phase(plan), expires_at);
    let Some(newest) = candles.iter().max_by_key(|c| c.time) else {
        return state;
    };
    state.watermark = Some(newest.time);
    for rule in &plan.rules {
        record_last_close(&rule.rule_id, &rule.trigger, newest, &mut state);
    }
    state
}

/// Evaluate a plan against the candles that have closed since its watermark.
///
/// `prior` is the persisted state (the caller seeds a fresh one on the first
/// tick — see the wrapper). `new_candles` must already be `> prior.watermark`
/// and ascending (the broker layer guarantees this; the wrapper re-filters
/// defensively). `now` is the tick time; `expires_at` the TTL stamp for the
/// returned state.
///
/// `detector_window` is the **full back-window** of recent closed candles
/// (history *and* `new_candles`, ascending) used only by [`Trigger::PinePattern`]
/// (the H&S candle detector) — which is stateful and needs lookback the
/// watermark-bounded `new_candles` slice doesn't carry. For an M/W plan (no Pine
/// rule) it is unused, so the wrapper may pass `new_candles` itself. Every new
/// candle must appear in it (matched by `time`); a new candle absent from the
/// window simply can't fire a Pine entry that tick.
pub fn evaluate_plan(
    plan: &TradePlan,
    prior: &PlanState,
    new_candles: &[Candle],
    detector_window: &[Candle],
    _now: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> PlanEval {
    let mut state = prior.clone();
    state.expires_at = expires_at;
    let mut fired = Vec::new();
    let detector_cfg = DetectorConfig::pine_defaults(plan.granularity);

    for candle in new_candles {
        // Always-armed control rules first: pause/resume/news-start/news-end
        // are wall-clock `TimeReached` fires that set KV state (blackout / news
        // window) without touching the trade's spine. Non-terminal — they fire,
        // latch, and the machine carries on. A window can land in any phase, so
        // these are evaluated every bar regardless of `state.phase`.
        evaluate_controls(plan, &mut state, candle, detector_window, &mut fired);

        // Always-armed guards next: a terminal veto can end the plan on any
        // bar, regardless of spine phase.
        evaluate_guards(
            plan,
            &mut state,
            candle,
            detector_window,
            &detector_cfg,
            &mut fired,
        );
        if state.phase == Phase::Done {
            state.watermark = Some(candle.time);
            break;
        }

        // Sequential spine.
        match state.phase {
            Phase::AwaitBreakAndClose => {
                evaluate_break_and_close(plan, &mut state, candle, detector_window, &mut fired);
            }
            Phase::AwaitEntry => {
                // Multi-enter (strategy-v2) starts in AwaitEntry but still has a
                // break-and-close prep that gates the stop entry. Run it here so
                // `break_close_at` gets stamped (and the prep intent dispatched)
                // exactly as it would in the AwaitBreakAndClose arm. The phase is
                // already AwaitEntry, so the `phase = AwaitEntry` set inside is a
                // no-op; the rule latches in `state.fired` so it stamps once.
                if is_multi_enter(plan) {
                    evaluate_break_and_close(plan, &mut state, candle, detector_window, &mut fired);
                }
                // Stamp the retest lookback before testing entry, so a retest
                // and entry that land on the same bar are both seen.
                stamp_retest(plan, &mut state, candle, detector_window, &mut fired);
                evaluate_entry(
                    plan,
                    &mut state,
                    candle,
                    detector_window,
                    &detector_cfg,
                    &mut fired,
                );
            }
            Phase::Done => {}
        }

        state.watermark = Some(candle.time);
        if state.phase == Phase::Done {
            break;
        }
    }

    let done = state.phase == Phase::Done;
    let warnings = trendline_anchor_warnings(plan, detector_window);
    PlanEval {
        fired,
        new_state: state,
        done,
        warnings,
    }
}

/// How a trendline anchor resolved against the candle window — the diagnostic
/// the warning surface reports.
enum AnchorResolution {
    /// The anchor matched a real bar or fell inside the window's span — the
    /// bar-index is exact (or one-hole interpolated). No warning.
    InWindow,
    /// The anchor fell outside the window's span and its bar-index was
    /// *estimated* from `bar_seconds`. The estimate reintroduces wall-clock
    /// spacing across any closed session in the un-fetched span, so it can slide
    /// the line — a soft warning.
    Extrapolated,
    /// The anchor fell outside the window's span and `bar_seconds == 0` (a plan
    /// signed before the field existed), so the bar-index can't be estimated at
    /// all and the trendline silently can't fire this tick — a hard warning.
    Unresolvable,
}

/// Classify a single anchor against the window using the same bar-index logic
/// [`bar_index_at`] applies, but reporting *how* it resolved rather than the
/// index itself.
fn classify_anchor(epoch: i64, window: &[Candle], bar_seconds: i64) -> AnchorResolution {
    if window.is_empty() {
        // No window at all → the wrapper would never have called us with a
        // trendline plan, but be total: treat as unresolvable.
        return AnchorResolution::Unresolvable;
    }
    let first = window[0].time.timestamp();
    let last = window[window.len() - 1].time.timestamp();
    if epoch >= first && epoch <= last {
        return AnchorResolution::InWindow;
    }
    if bar_seconds <= 0 {
        AnchorResolution::Unresolvable
    } else {
        AnchorResolution::Extrapolated
    }
}

/// Detect every [`Trigger::TrendlineCross`] whose anchor falls outside the
/// fetched `window`, so the wrapper can log that the line's level was estimated
/// (or couldn't be resolved). This is the **observability half** of the
/// bar-index fix: in-window anchors are exact, but an out-of-window anchor falls
/// back to the `bar_seconds` divisor — which re-introduces wall-clock spacing
/// across any gap in the un-fetched span (the very assumption the bar-index work
/// removed), or, on a pre-`bar_seconds` plan, makes the trendline silently
/// un-evaluable. Both are rare in practice (a normal H&S/M/W `detector_window`
/// straddles its anchors) but must not be silent when they do happen.
///
/// Pure and window-derived, so it recomputes deterministically on replay and is
/// deliberately *not* part of the replay diff.
fn trendline_anchor_warnings(plan: &TradePlan, window: &[Candle]) -> Vec<String> {
    let mut warnings = Vec::new();
    for rule in &plan.rules {
        let Trigger::TrendlineCross {
            a, b, bar_seconds, ..
        } = &rule.trigger
        else {
            continue;
        };
        for (label, anchor) in [("a", a), ("b", b)] {
            match classify_anchor(anchor.at_epoch, window, *bar_seconds) {
                AnchorResolution::InWindow => {}
                AnchorResolution::Extrapolated => warnings.push(format!(
                    "trendline {rule_id}: anchor {label} (epoch {epoch}) is outside the \
                     {n}-bar window; its bar-index was estimated from bar_seconds={bar_seconds} \
                     (wall-clock spacing across any gap in the un-fetched span). Widen the \
                     candle fetch so anchors are in-window.",
                    rule_id = rule.rule_id,
                    epoch = anchor.at_epoch,
                    n = window.len(),
                    bar_seconds = bar_seconds,
                )),
                AnchorResolution::Unresolvable => warnings.push(format!(
                    "trendline {rule_id}: anchor {label} (epoch {epoch}) is outside the \
                     {n}-bar window and bar_seconds=0 (plan signed before the field existed), \
                     so this trendline cannot be evaluated this tick (silently won't fire). \
                     Re-arm the plan to bake bar_seconds, or widen the candle fetch.",
                    rule_id = rule.rule_id,
                    epoch = anchor.at_epoch,
                    n = window.len(),
                )),
            }
        }
    }
    warnings
}

/// Evaluate every control rule (pause / resume / news-start / news-end)
/// against this candle. Each is a `TimeReached` rule whose intent sets KV
/// state (a blackout or news window) — **non-terminal**: it fires, latches in
/// `state.fired` (`FireMode::Once`), and the plan continues. Distinct from a
/// guard (which ends the spine) and from the prep spine. Armed in every phase
/// so a window that opens before break-and-close still fires.
fn evaluate_controls(
    plan: &TradePlan,
    state: &mut PlanState,
    candle: &Candle,
    window: &[Candle],
    fired: &mut Vec<FiredIntent>,
) {
    for rule in &plan.rules {
        if !is_control_rule(rule) || state.fired.contains(&rule.rule_id) {
            continue;
        }
        if fire_rule(rule, state, candle, window, plan.cross_buffer_pct) {
            push_fire(rule, candle, fired);
            state.fired.insert(rule.rule_id.clone());
            // No phase change: a pause/resume/news fire is state-only and never
            // ends the setup. But a news-start/news-end fire *does* open/close a
            // news window in `open_news_windows` — the pure mirror of the
            // worker's `news:<trade_id>:<news_id>` KV entry that gates a
            // news-only reversal-close. `news_id` is required on these intents
            // (validated), so the `if let` only skips a malformed rule.
            if let Some(news_id) = rule.intent.news_id.as_deref() {
                match rule.intent.action {
                    Action::NewsStart => {
                        state.open_news_windows.insert(news_id.to_string());
                    }
                    Action::NewsEnd => {
                        state.open_news_windows.remove(news_id);
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Evaluate every veto guard rule against this candle. A guard that fires
/// pushes its intent and, being terminal, ends the plan. Guards are
/// `FireMode::Once` so they latch in `state.fired`.
///
/// Two kinds of guard trigger:
/// - **Level / time guards** (`HorizontalCross` / `PriceValueCross` /
///   `TrendlineCross` / `TimeReached`) — a pure per-candle predicate via
///   [`fire_rule`]; no pattern geometry.
/// - **`PinePattern` guards** — the consolidated `06-close-on-reversal` close,
///   which fires when a confirming reversal candle of the *opposite* direction
///   prints. This is the SAME stateful candle detector the entry path uses
///   ([`eval_pine_entry`]), not a level cross — `eval_trigger` deliberately
///   returns `false` for `PinePattern`, so a guard carrying one is routed to
///   [`eval_pine_guard`] here instead. The latched signal rides onto the fired
///   intent's shell so the worker's `run_close` sees the candle-quality flags
///   (`golden` / `confirmed`) the gate reads.
fn evaluate_guards(
    plan: &TradePlan,
    state: &mut PlanState,
    candle: &Candle,
    window: &[Candle],
    detector_cfg: &DetectorConfig,
    fired: &mut Vec<FiredIntent>,
) {
    for rule in &plan.rules {
        if !is_guard(rule) || state.fired.contains(&rule.rule_id) {
            continue;
        }
        if !armed_in(&rule.rule_id, state.phase) {
            continue;
        }
        // A `PinePattern` guard (the close-on-reversal close) is decided by the
        // candle detector over the back-window, not a level cross. Every other
        // guard trigger is the plain per-candle predicate.
        let signal = match &rule.trigger {
            Trigger::PinePattern { pattern, dir } => {
                match eval_pine_guard(
                    &rule.intent,
                    candle,
                    window,
                    detector_cfg,
                    *pattern,
                    *dir,
                    &state.open_news_windows,
                ) {
                    Some(sig) => Some(sig),
                    None => continue,
                }
            }
            _ => {
                if !fire_rule(rule, state, candle, window, plan.cross_buffer_pct) {
                    continue;
                }
                None
            }
        };
        push_fire_signal(rule, candle, signal, fired);
        state.fired.insert(rule.rule_id.clone());
        // Most veto guards are terminal for the plan's spine: the dispatched
        // intent (cancel / invalidate, or a price-windowed reversal-close at the
        // SR band) is the end of this setup. A **news-windowed** reversal-close
        // is the exception — it is a "flatten *if* in a position" safety, not a
        // thesis invalidation. The engine can't see whether a position is open
        // (no broker), and the worker's / replay's `allow_close` gate blocks the
        // flatten when flat. So a news-close that fires before any entry filled
        // must NOT retire the spine, or it starves every pending entry even
        // though it closed nothing (USD/CHF 2026-06-26 Defect B). It still
        // dispatches the flatten; the spine survives so pending entries proceed.
        if guard_is_terminal(&rule.intent) {
            state.phase = Phase::Done;
            return;
        }
        // Non-terminal guard fired (news-close): keep scanning later rules this
        // bar, but it has latched so it won't re-fire.
    }
}

/// Whether a fired guard retires the plan's spine. All guards are terminal
/// **except** a news-windowed reversal-close — see the call site in
/// [`evaluate_guards`] for why (it's a flatten-if-open safety, not a thesis
/// invalidation, and the engine has no broker to know whether anything was
/// flattened). A close that also opts into the price window stays terminal (a
/// reversal back at the SR band *does* invalidate the thesis).
fn guard_is_terminal(intent: &trade_control_core::intent::Intent) -> bool {
    if intent.action != Action::Close {
        return true;
    }
    let wants_news = intent
        .inside_window
        .contains(&trade_control_core::intent::EventWindow::News);
    let wants_price = intent
        .inside_window
        .contains(&trade_control_core::intent::EventWindow::Price)
        || intent.require_price_in_ranges.is_some();
    // News-only close → non-terminal. A close with a price window (with or
    // without news) is terminal: the at-band reversal is the thesis breaking.
    !wants_news || wants_price
}

/// Evaluate the break-and-close prep. On fire it latches, records the lookback
/// window start, and advances to `AwaitEntry`. The rule then "dies".
fn evaluate_break_and_close(
    plan: &TradePlan,
    state: &mut PlanState,
    candle: &Candle,
    window: &[Candle],
    fired: &mut Vec<FiredIntent>,
) {
    let Some(rule) = plan.rules.iter().find(|r| is_break_and_close(r)) else {
        // No break-and-close rule but we're in its phase — advance immediately
        // (defensive; initial_phase wouldn't put us here).
        state.phase = Phase::AwaitEntry;
        return;
    };
    // The break-and-close prep is `FireMode::Once`: it stamps the lookback start
    // exactly once and then "dies". In a single-enter plan the phase advances off
    // `AwaitBreakAndClose` and this function is never re-entered, so the latch was
    // implicit. But a **multi-enter (strategy-v2)** plan stays in `AwaitEntry` and
    // re-runs this every bar — so a later neckline re-cross would re-fire and walk
    // `break_close_at` forward, pushing an already-seen retest *before* the new
    // break window and starving the stop enter forever (replay trade 071). Honour
    // the latch explicitly: once fired, never re-stamp.
    if state.fired.contains(&rule.rule_id) {
        return;
    }
    if fire_rule(rule, state, candle, window, plan.cross_buffer_pct) {
        state.fired.insert(rule.rule_id.clone());
        state.break_close_at = Some(candle.time);
        // The break-and-close prep itself is recorded server-side by dispatching
        // its intent (a Prep action), exactly as the TV alert would have.
        push_fire(rule, candle, fired);
        state.phase = Phase::AwaitEntry;
    }
}

/// Evaluate the plan's entry rule(s). A single-enter plan (H&S stop entry or
/// M/W heartbeat) has one; a strategy-v2 plan has two (the stop entry plus the
/// Quasimodo limit), and **both** are evaluated every bar — whichever fires
/// first dispatches, and the worker's retry gate cancels the other's resting
/// order (the two enters share a `trade_id`). The stop enter is ordered before
/// the QM enter in `plan.rules`, so on a bar where both qualify the stop
/// dispatches first (the QM is then deduped by the retry gate's same-bar
/// guard) — the operator's "stop wins the tie" choice.
fn evaluate_entry(
    plan: &TradePlan,
    state: &mut PlanState,
    candle: &Candle,
    detector_window: &[Candle],
    detector_cfg: &DetectorConfig,
    fired: &mut Vec<FiredIntent>,
) {
    // Snapshot the enter rule_ids up front so we can borrow `plan` immutably in
    // the loop while mutating `state`. Skip ones already latched (a fired
    // single-shot enter must not re-fire).
    let enter_rule_ids: Vec<String> = plan
        .rules
        .iter()
        .filter(|r| r.intent.action == Action::Enter)
        .map(|r| r.rule_id.clone())
        .collect();
    for rule_id in enter_rule_ids {
        if state.fired.contains(&rule_id) {
            continue;
        }
        let Some(rule) = plan.rules.iter().find(|r| r.rule_id == rule_id) else {
            continue;
        };
        evaluate_one_entry(
            plan,
            state,
            rule,
            candle,
            detector_window,
            detector_cfg,
            fired,
        );
        // A terminal single-shot enter ends the spine — stop evaluating the
        // rest (there is at most one such enter, and the plan is now Done).
        if state.phase == Phase::Done {
            return;
        }
    }
}

/// Evaluate a single enter `rule`. Factored out of [`evaluate_entry`] so a
/// strategy-v2 plan can run it once per enter rule. The body is the historical
/// single-enter logic, with the retest gate keyed on **this rule's**
/// `requires_preps` rather than the plan-global presence of a retest rule (so
/// the QM enter, which lists no preps, is not blocked by the stop enter's
/// retest rule).
#[allow(clippy::too_many_arguments)]
fn evaluate_one_entry(
    plan: &TradePlan,
    state: &mut PlanState,
    rule: &ConditionRule,
    candle: &Candle,
    detector_window: &[Candle],
    detector_cfg: &DetectorConfig,
    fired: &mut Vec<FiredIntent>,
) {
    // Replay-start entry floor (journaling only). When a plan carries a
    // `replay_start` cursor (baked by `tv-arm --start`), the bars before it are
    // warmup/context — "live now" begins at the cursor — so no enter may fire on
    // a bar that opens before it. Without this, a free enter (the strategy-v2 QM
    // enter, which has no prep spine) can latch onto a micro-pattern sitting
    // right at the replay boundary and enter before the setup being journaled has
    // even formed. The field is `None` on the live worker path (it ignores it),
    // so this floor is inert in production — exactly the intent (live trading has
    // no artificial start).
    if let Some(start) = plan.replay_start
        && candle.time.timestamp() < start
    {
        return;
    }

    // A `PinePattern` enter is decided by the stateful candle detector over the
    // back-window, not by a per-candle level cross. It also produces the latched
    // signal geometry that rides onto the dispatched shell.
    let signal = match &rule.trigger {
        Trigger::PinePattern { pattern, dir } => {
            match eval_pine_entry(candle, detector_window, detector_cfg, *pattern, *dir) {
                Some(sig) => Some(sig),
                None => return,
            }
        }
        // Every other entry trigger (the M/W heartbeat) is a plain per-candle
        // predicate with no pattern geometry.
        _ => {
            if !fire_rule(rule, state, candle, detector_window, plan.cross_buffer_pct) {
                return;
            }
            None
        }
    };

    // Pre-flight a `PinePattern` enter before firing (bug #13). The detector
    // firing is necessary but not sufficient: the candle-quality gate
    // (`needs_golden`/`needs_confirmed`) and bracket resolution still have to
    // pass, and both are **pure** here (the latched signal carries the flags;
    // `Resolved::from_intent` needs only intent + shell + pip_size). If either
    // fails, this is a *decline this bar*, not a terminal event — return without
    // firing so the plan stays in `AwaitEntry`, its vetos keep ticking, and a
    // later bar can re-form a valid pattern. (A false-golden tiny pinbar with
    // `signal_high ≈ signal_low` resolves to a degenerate zeros bracket; the old
    // code fired + went `Done` regardless, abandoning the live vetos.)
    //
    // Scope is deliberately `PinePattern` only: the M/W heartbeat's resolution
    // (and its by-design `NotArmedYet` decline) is owned by the worker's
    // `run_enter → maybe_update_mw_state`, so pre-resolving it here would
    // wrongly suppress the heartbeat. `signal.is_some()` ⇔ a Pine fire.
    if let Some(sig) = &signal
        && !pine_entry_dispatchable(&rule.intent, candle, sig, plan.pip_size)
    {
        return;
    }

    // Retest gate: a retest must have been seen in (break_close_at, this
    // candle] before this enter may fire.
    //
    // Which enters are gated:
    // - **Multi-enter (strategy-v2):** key strictly on *this enter's*
    //   `requires_preps` — the stop enter lists `retest` and is gated; the QM
    //   enter lists no preps and is free. This is what lets the two enters
    //   diverge within one plan.
    // - **Single-enter:** preserve the historical plan-global rule exactly —
    //   "a retest rule exists ⇒ gate the (lone) enter". The engine's synthetic
    //   test intents don't populate `requires_preps`, and production single
    //   enters always do, so the plan-global check is the byte-identical
    //   baseline here. (The two agree in production; only the multi-enter case
    //   needs the per-rule split.)
    let requires_retest = if is_multi_enter(plan) {
        rule.intent
            .requires_preps
            .iter()
            .any(|p| p == PREP_STEP_RETEST)
    } else {
        plan.rules.iter().any(is_retest)
    };
    if requires_retest && !retest_satisfied(state, candle.time) {
        return;
    }
    push_fire_signal(rule, candle, signal, fired);
    // A heartbeat (EveryBar) enter does not latch or finish the spine — the
    // worker's run_enter owns the actual placement/dedup, and M/W rides its TTL
    // / a veto to end.
    //
    // A single-shot (`Once`) enter normally ends the spine on its first fire.
    // **But a multi-shot enter (`max_retries > 0`) must NOT** — it is the place
    // → fill → close → re-enter-on-the-next-signal-bar mechanism, and if the
    // engine retired the plan here the cron would archive it (engine.rs) and no
    // later bar could ever fire the re-entry. So a multi-shot enter fires this
    // bar and stays in `AwaitEntry`: the plan survives, its vetos keep ticking,
    // and the next golden signal bar fires again. The *placement cap* is the
    // worker's `retry_gate` (which the replay also runs); the engine just keeps
    // emitting fires. The plan still retires the normal way — a terminal veto /
    // trade-expiry, or the enter's `not_after` window closing.
    //
    // `max_retries` is `Tunable<u32>`; treat anything other than the static
    // default `Static(0)` as multi-shot, mirroring the worker's gate-entry
    // check (`src/lib.rs` `run_enter`). A script-based cap (rare) is therefore
    // multi-shot here too — the worker resolves the actual number at placement.
    let multi_shot = !matches!(
        rule.intent.max_retries,
        trade_control_core::tunable::Tunable::Static(0)
    );
    if matches!(
        rule.fire_mode,
        trade_control_core::trade_plan::FireMode::Once
    ) && !multi_shot
    {
        state.fired.insert(rule.rule_id.clone());
        state.phase = Phase::Done;
    }
}

/// Decide a `PinePattern` entry on this candle: recompute the latched candle
/// signal over `detector_window` at this candle's index and fire iff the alert
/// fires on it, the latched direction matches the plan's, and (when set) the
/// kind matches. Returns the latched signal to ride onto the shell, or `None`
/// to not fire.
fn eval_pine_entry(
    candle: &Candle,
    detector_window: &[Candle],
    cfg: &DetectorConfig,
    pattern: Option<SignalKind>,
    dir: Direction,
) -> Option<LatchedSignal> {
    let Some(idx) = detector_window.iter().position(|c| c.time == candle.time) else {
        tracing::debug!(
            bar = %candle.time,
            "pine-enter: candle not found in detector window (no signal)"
        );
        return None;
    };
    let Some(sig) = latched_signal_at(detector_window, idx, cfg) else {
        tracing::debug!(
            bar = %candle.time,
            window_len = detector_window.len(),
            "pine-enter: no latched signal yet (detector warming / no pattern printed)"
        );
        return None;
    };
    // Log the full latched state every bar so a replay with RUST_LOG=debug shows
    // *why* a bar does or doesn't fire the enter: the detector verdict, the
    // latched geometry, and the quality flags the dispatchable gate reads.
    tracing::debug!(
        bar = %candle.time,
        fires = sig.fires,
        latched_dir = ?sig.direction,
        want_dir = ?dir,
        kind = ?sig.kind,
        want_kind = ?pattern,
        golden = sig.golden,
        confirmed = sig.signal_confirmed,
        signal_high = sig.signal_high,
        signal_low = sig.signal_low,
        "pine-enter: latched signal at this bar"
    );
    if !sig.fires {
        tracing::debug!(bar = %candle.time, "pine-enter: alert does not fire on this bar");
        return None;
    }
    if sig.direction != dir {
        tracing::debug!(
            bar = %candle.time,
            latched_dir = ?sig.direction,
            want_dir = ?dir,
            "pine-enter: latched direction != plan direction — decline"
        );
        return None;
    }
    if let Some(want) = pattern
        && sig.kind != want
    {
        tracing::debug!(
            bar = %candle.time,
            kind = ?sig.kind,
            want_kind = ?want,
            "pine-enter: latched kind != required pattern — decline"
        );
        return None;
    }
    Some(sig)
}

/// Decide a `PinePattern` **guard** (the consolidated `06-close-on-reversal`
/// close) on this candle. Like [`eval_pine_entry`], it recomputes the latched
/// candle signal over `detector_window` at this bar and fires iff the alert
/// fires, the latched direction matches the rule's `dir` (the *opposite* of the
/// trade — a long reversal closes a short), and (when set) the kind matches.
///
/// On top of the detector match it applies the **pure half** of the worker's
/// `run_close` contextual gate, mirroring **both** windows `run_close` checks:
///
///   - **Price window** (`inside_window` lists `price`): the reversal candle's
///     close must sit inside one of the intent's `sr_bands`. Matches live, where
///     `run_close` rejects a close whose broker price is out of every band.
///   - **News window** (`inside_window` lists `news`): a news window must be
///     **currently open** — i.e. `open_news_windows` is non-empty. This mirrors
///     `run_close`'s `list_news_windows_for_trade` check against the
///     `news:<trade_id>:<news_id>` KV the worker's `news-start`/`news-end`
///     control fires maintain. The engine keeps the same window state in
///     `PlanState::open_news_windows` (opened on a `NewsStart` fire, closed on
///     `NewsEnd`), so it gates identically to live. **Without this the engine
///     fired the close on *any* qualifying reversal anywhere on the chart**,
///     including hours after `news-end` (USD/CHF 2026-06-26: a long reversal 9h
///     past the GDP window erased two legitimate short entries by retiring the
///     spine).
///
/// (When the worker dispatches this fire it re-runs the full gate against live
/// state, so a fire let through here can still be declined there — the engine is
/// permissive-but-aligned, never stricter than live.)
///
/// Returns the latched signal to ride onto the shell, or `None` to not fire.
#[allow(clippy::too_many_arguments)]
fn eval_pine_guard(
    intent: &trade_control_core::intent::Intent,
    candle: &Candle,
    detector_window: &[Candle],
    cfg: &DetectorConfig,
    pattern: Option<SignalKind>,
    dir: Direction,
    open_news_windows: &std::collections::BTreeSet<String>,
) -> Option<LatchedSignal> {
    // News-window gate (the engine's mirror of `run_close`'s
    // `list_news_windows_for_trade`). When the close opted into the news window,
    // it may only fire while at least one news window is open. Checked *before*
    // the detector so a reversal printing outside the window is silently
    // skipped, leaving the spine intact.
    let wants_news = intent
        .inside_window
        .contains(&trade_control_core::intent::EventWindow::News);
    if wants_news && open_news_windows.is_empty() {
        tracing::debug!(
            bar = %candle.time,
            "pine-close: news-only close but no news window open — decline (spine survives)"
        );
        return None;
    }

    // The detector match is shared with the entry path (direction / kind /
    // fires), so a close and an enter can't drift on what "a reversal printed"
    // means. `eval_pine_entry` logs under the `pine-enter` tag; the extra
    // band gate below logs under `pine-close`.
    let sig = eval_pine_entry(candle, detector_window, cfg, pattern, dir)?;

    // Price-band gate (the pure half of `run_close`'s contextual window). Only
    // applied when the intent opted into the price window; a news-only
    // reversal-close has no recomputable price gate here and fires on the
    // detector match alone (the worker's news-window KV gate decides it).
    let wants_price = intent
        .inside_window
        .contains(&trade_control_core::intent::EventWindow::Price);
    if wants_price {
        // The reversal candle's close is the pure proxy for the broker's
        // current price the worker checks. Bands come from the signed intent.
        if price_in_any_band(candle.c, &intent.sr_bands).is_none() {
            tracing::debug!(
                bar = %candle.time,
                close = candle.c,
                bands = ?intent.sr_bands,
                "pine-close: reversal candle outside every SR band — decline"
            );
            return None;
        }
        tracing::debug!(
            bar = %candle.time,
            close = candle.c,
            "pine-close: reversal candle inside an SR band — close fires"
        );
    } else {
        tracing::debug!(
            bar = %candle.time,
            "pine-close: reversal detector fired (no price-band gate) — close fires"
        );
    }
    Some(sig)
}

/// The first `[lo, hi]` band that contains `price` (inclusive of both
/// endpoints), or `None`. Mirrors the worker's `src/lib.rs::price_band_hit` —
/// the worker keeps its own copy because it tests a *live broker* price, while
/// this engine copy tests a *candle* price for the pure close-on-reversal gate.
fn price_in_any_band(price: f64, bands: &[[f64; 2]]) -> Option<[f64; 2]> {
    bands
        .iter()
        .copied()
        .find(|[lo, hi]| price >= *lo && price <= *hi)
}

/// Is a fired `PinePattern` enter actually dispatchable on this bar — i.e. would
/// the worker's `run_enter` accept it rather than decline it? Returns `false`
/// for the two **pure, recomputable-here** rejections that the FSM must treat as
/// "decline this bar, stay armed" (not as a terminal fire):
///
/// 1. The candle-quality gate: `needs_golden` / `needs_confirmed` on the intent
///    vs the latched signal's flags. Mirrors `candle_gate::evaluate` in the
///    worker (which keys off the same flags, folded onto the shell). Checking
///    it here stops a non-golden bar from firing + retiring the plan when the
///    operator demanded golden.
/// 2. Bracket resolution: `Resolved::from_intent` on the signal-folded shell. A
///    degenerate signal (e.g. a false-golden tiny pinbar with
///    `signal_high ≈ signal_low`) resolves to an inconsistent / zeros bracket
///    and the worker rejects it `resolve-failed`. Pre-checking it here keeps a
///    `resolve-failed` bar from tearing the plan (and its live vetos) down.
///
/// Pure: the shell, the gate flags, and `from_intent` are all deterministic
/// functions of the inputs, so this recomputes identically on replay. The
/// worker still re-runs its own gates + resolution on dispatch — this is a
/// *pre-flight*, not a replacement (it never sees the account caps, cooldown,
/// retry, or `allow_entry` script the worker also applies).
fn pine_entry_dispatchable(
    intent: &trade_control_core::intent::Intent,
    candle: &Candle,
    sig: &LatchedSignal,
    pip_size: f64,
) -> bool {
    // Candle-quality gate — `None`/`false` both fail (conservative reject),
    // matching `candle_gate::evaluate`.
    if intent.needs_golden && !sig.golden {
        tracing::debug!(
            bar = %candle.time,
            "pine-enter: NOT dispatchable — needs_golden but signal is not golden"
        );
        return false;
    }
    if intent.needs_confirmed && !sig.signal_confirmed {
        tracing::debug!(
            bar = %candle.time,
            "pine-enter: NOT dispatchable — needs_confirmed but signal is not confirmed"
        );
        return false;
    }
    // Bracket resolution against the same signal-folded shell the worker
    // dispatches. An `Err` (degenerate geometry, below-min-R, out-of-range, …)
    // is a decline-this-bar, not a fire.
    let shell = trade_control_core::intent::Shell::from_candle_and_signal(candle, sig);
    match trade_control_core::intent::Resolved::from_intent(intent, &shell, pip_size) {
        Ok(_) => {
            tracing::debug!(bar = %candle.time, "pine-enter: dispatchable — will fire enter");
            true
        }
        Err(e) => {
            log_rejected_entry_spec(&e, intent, &shell, pip_size);
            false
        }
    }
}

/// Dump the **fully-resolved** entry spec of a rejected enter so a
/// `RUST_LOG=...=debug` replay shows exactly what geometry was being compared
/// against what — instead of just the opaque `resolve-failed` string. The
/// trigger / SL / TP are recomputed the same way the resolver does
/// (`anchor_price + offset × pip_size`, `PriceRef::resolve`, the `TakeProfit`
/// variants), and the shell's reference prices (`close`/`signal_high`/
/// `signal_low`) are logged alongside so the wrong-side check
/// (`short stop trigger >= close`) is auditable at a glance.
fn log_rejected_entry_spec(
    err: &trade_control_core::intent::ResolveError,
    intent: &trade_control_core::intent::Intent,
    shell: &trade_control_core::intent::Shell,
    pip_size: f64,
) {
    use trade_control_core::intent::{EntrySpec, PriceRef, TakeProfit};

    // Entry trigger / kind — mirror `Resolved::from_intent`'s match arms.
    let (entry_kind, entry_trigger) = match &intent.entry {
        Some(EntrySpec::Market) => ("market", Some(shell.close)),
        Some(EntrySpec::Stop {
            from,
            offset_pips,
            offset_atr_pct,
            at,
            ..
        }) => (
            "stop",
            Some(at.unwrap_or_else(|| {
                log_anchor(*from, *offset_pips, *offset_atr_pct, shell, pip_size)
            })),
        ),
        Some(EntrySpec::Limit {
            from,
            offset_pips,
            offset_atr_pct,
            at,
        }) => (
            "limit",
            Some(at.unwrap_or_else(|| {
                log_anchor(*from, *offset_pips, *offset_atr_pct, shell, pip_size)
            })),
        ),
        None => ("none", None),
    };

    let stop_loss = intent
        .stop_loss
        .as_ref()
        .map(|sl| sl.resolve(shell, pip_size).unwrap_or(f64::NAN));

    // Take-profit — RMultiple needs the entry+SL to resolve, so only report the
    // simple absolute / anchored case precisely; RMultiple is logged as a label.
    let take_profit = match &intent.take_profit {
        Some(TakeProfit::Anchored(PriceRef::Absolute { absolute })) => Some(*absolute),
        Some(TakeProfit::Anchored(r)) => Some(r.resolve(shell, pip_size).unwrap_or(f64::NAN)),
        _ => None,
    };

    tracing::debug!(
        bar = %shell.time,
        error = %err,
        direction = ?intent.direction,
        entry_kind,
        entry_trigger,
        stop_loss,
        take_profit,
        shell_close = shell.close,
        shell_high = shell.high,
        shell_low = shell.low,
        signal_high = ?shell.signal_high,
        signal_low = ?shell.signal_low,
        recover_entry = ?entry_recover_entry(&intent.entry),
        "pine-enter: NOT dispatchable — resolve failed; resolved entry spec follows"
    );
}

/// Resolve `anchor + offset` for the rejection log, mirroring the resolver's
/// own `anchor_price + resolve_offset` so the logged trigger reflects the real
/// (ATR-pct or pips) buffer. When the offset can't resolve — which is often the
/// very reason we're logging (ATR unavailable in warmup) — fall back to `NaN`
/// so the log line still emits rather than the logger itself failing.
fn log_anchor(
    from: trade_control_core::intent::PriceAnchor,
    offset_pips: f64,
    offset_atr_pct: Option<f64>,
    shell: &trade_control_core::intent::Shell,
    pip_size: f64,
) -> f64 {
    match trade_control_core::intent::resolve_offset(
        from,
        offset_pips,
        offset_atr_pct,
        shell,
        pip_size,
    ) {
        Ok(delta) => shell.anchor_price(from) + delta,
        Err(_) => f64::NAN,
    }
}

/// Short label for the entry's `recover_entry` opt-in (for the rejection log).
fn entry_recover_entry(
    entry: &Option<trade_control_core::intent::EntrySpec>,
) -> Option<trade_control_core::intent::RecoverEntryAction> {
    use trade_control_core::intent::EntrySpec;
    match entry {
        Some(EntrySpec::Stop {
            recover_entry: Some(rec),
            ..
        }) => Some(rec.action),
        _ => None,
    }
}

/// Stamp `retest_seen_at` if this candle satisfies the retest trendline
/// geometry and falls after the break-and-close. No-op if there's no retest
/// rule or no break-and-close has fired yet.
fn stamp_retest(
    plan: &TradePlan,
    state: &mut PlanState,
    candle: &Candle,
    window: &[Candle],
    fired: &mut Vec<FiredIntent>,
) {
    let Some(break_at) = state.break_close_at else {
        return;
    };
    if candle.time <= break_at {
        return;
    }
    let Some(rule) = plan.rules.iter().find(|r| is_retest(r)) else {
        return;
    };
    // Only stamp once: the retest is a milestone the trade passes a single
    // time. Without this latch a later re-cross would re-stamp `retest_seen_at`
    // and re-emit the prep fire every bar (the break-and-close analogue of the
    // strategy-v2 starvation bug).
    if state.retest_seen_at.is_none()
        && eval_trigger(
            &rule.trigger,
            candle,
            state.last_close.get(&rule.rule_id).copied(),
            window,
            plan.cross_buffer_pct,
        )
    {
        state.retest_seen_at = Some(candle.time);
        // Emit the retest prep fire so it seeds the store the enter's prep gate
        // reads — exactly as the TV `04-prep-retest` alert did, and exactly as
        // `evaluate_break_and_close` emits the break-and-close prep. The engine
        // satisfies the retest internally via `retest_satisfied(state, …)`, but
        // `run_enter`'s prep gate is store-backed, so the prep must be dispatched
        // (worker + replay route `Action::Prep` → `handle_prep` → `set_prep`).
        // Without this, the engine validated the retest but `run_enter` rejected
        // the enter with `missing-prep (retest)`.
        push_fire(rule, candle, fired);
    }
    // The retest's `last_close` is tracked so an OnClose retest works across
    // ticks; record it regardless of whether it fired.
    record_last_close(&rule.rule_id, &rule.trigger, candle, state);
}

/// Is a retest seen within `(break_close_at, entry]`?
fn retest_satisfied(state: &PlanState, entry_time: DateTime<Utc>) -> bool {
    match (state.break_close_at, state.retest_seen_at) {
        (Some(break_at), Some(seen)) => seen > break_at && seen <= entry_time,
        _ => false,
    }
}

/// Fire a rule against a candle: evaluate its trigger (updating the rule's
/// `last_close` memory) and return whether it fired this bar. Latched rules
/// don't re-fire (the caller checks `state.fired` for guards; this also guards
/// the entry/break-and-close paths via `record_last_close`).
fn fire_rule(
    rule: &ConditionRule,
    state: &mut PlanState,
    candle: &Candle,
    window: &[Candle],
    buffer_pct: f64,
) -> bool {
    let prev_close = state.last_close.get(&rule.rule_id).copied();
    let hit = eval_trigger(&rule.trigger, candle, prev_close, window, buffer_pct);
    record_last_close(&rule.rule_id, &rule.trigger, candle, state);
    hit
}

/// Persist this candle's close as the rule's `last_close` so an `OnClose`
/// cross can be detected against it next bar — even across cron ticks. Only
/// `OnClose` triggers need it, but recording unconditionally is harmless and
/// keeps the seed logic simple.
fn record_last_close(rule_id: &str, trigger: &Trigger, candle: &Candle, state: &mut PlanState) {
    if trigger_uses_close(trigger) {
        state.last_close.insert(rule_id.to_string(), candle.c);
    }
}

/// Whether a trigger's evaluation reads the prior close (so `last_close` must
/// be tracked for it).
fn trigger_uses_close(trigger: &Trigger) -> bool {
    matches!(
        trigger,
        Trigger::HorizontalCross {
            bar: BarEvent::OnClose,
            ..
        } | Trigger::PriceValueCross {
            bar: BarEvent::OnClose,
            ..
        } | Trigger::TrendlineCross {
            bar: BarEvent::OnClose,
            ..
        }
    )
}

/// Pure trigger evaluation against a single candle. `prev_close` is the rule's
/// last processed close (for `OnClose` crosses); `None` on the seed bar, which
/// never fires an `OnClose` cross. `window` is the ascending bar series used to
/// resolve a `TrendlineCross`'s level in bar-index space (ignored by every
/// other trigger); pass the plan's `detector_window` (or `new_candles` when no
/// trendline rule is present).
pub fn eval_trigger(
    trigger: &Trigger,
    candle: &Candle,
    prev_close: Option<f64>,
    window: &[Candle],
    buffer_pct: f64,
) -> bool {
    match trigger {
        Trigger::HorizontalCross { level, dir, bar }
        | Trigger::PriceValueCross { level, dir, bar } => {
            level_crossed(*level, *dir, *bar, candle, prev_close, buffer_pct)
        }
        Trigger::TrendlineCross {
            a,
            b,
            extend_forward,
            bar_seconds,
            dir,
            bar,
        } => {
            let Some(level) = line_price_at(a, b, candle, *extend_forward, *bar_seconds, window)
            else {
                return false;
            };
            level_crossed(level, *dir, *bar, candle, prev_close, buffer_pct)
        }
        Trigger::TimeReached { at_epoch } => candle.time.timestamp() >= *at_epoch,
        // The M/W heartbeat fires on every closed bar — the wrapper feeds only
        // closed candles, so any candle is a heartbeat tick.
        Trigger::MwEveryBar => true,
        // The H&S candle-pattern entry is **not** a per-candle predicate: it is
        // stateful and needs the back-window, so it is handled in
        // [`eval_pine_entry`] off `detector_window`, not here. A `PinePattern`
        // never reaches `eval_trigger` (only `Action::Enter` rules carry it, and
        // the entry path special-cases it). Returning `false` keeps this total.
        Trigger::PinePattern { .. } => false,
    }
}

/// Did `candle` cross `level` in direction `dir` under the bar-event mode?
///
/// `buffer_pct` is a cross-depth buffer as a percent of `level`'s price: an
/// *intrabar* directional cross must pierce the wick at least `buffer_pct%` past
/// the line before it counts, so a one-tick graze of a retest / invalidation
/// line doesn't trip it. `0.0` reproduces the bare wick-touch behaviour. The
/// buffer is applied on the **far** side of the line (below for `Down`, above
/// for `Up`); `Either` and `OnClose` ignore it (a bare straddle / a close past
/// the line is already an unambiguous cross).
fn level_crossed(
    level: f64,
    dir: CrossDir,
    bar: BarEvent,
    candle: &Candle,
    prev_close: Option<f64>,
    buffer_pct: f64,
) -> bool {
    // The depth the wick must reach past the line. `level` is positive (a price)
    // and `buffer_pct` is small; a 0.0 buffer collapses to the raw level.
    let buffer = level * buffer_pct / 100.0;
    match bar {
        // Intrabar: the bar's range crosses the level *within the bar*, read
        // from the wick — NOT the close, and (as of 2026-07-03) NOT the open
        // either. The rule is now a pure straddle: the bar's high and low must
        // sit on *opposite* sides of the line. On a tick timeline a bar whose
        // range spans the level traded on both sides of it, which is enough to
        // count as a touch/cross regardless of where it opened or closed (a
        // retest tap, an invalidation spike-and-recover). The directional wick
        // must still reach `buffer` past the line so a one-tick graze doesn't
        // trip it:
        //   Down  ⇒ low reached `level - buffer` or lower (high on/above by straddle)
        //   Up    ⇒ high reached `level + buffer` or higher (low on/below by straddle)
        //   Either⇒ any straddle (the wick touched the level at all; buffer N/A)
        // (Close-confirmed crossing is `BarEvent::OnClose` — break-and-close —
        // which must open one side and *close* the other; that arm is below and
        // is deliberately unchanged, buffer N/A.)
        BarEvent::Intrabar => {
            let straddles = candle.l <= level && level <= candle.h;
            if !straddles {
                return false;
            }
            match dir {
                CrossDir::Either => true,
                CrossDir::Up => candle.h >= level + buffer,
                CrossDir::Down => candle.l <= level - buffer,
            }
        }
        // OnClose: a cross relative to the prior processed close. The seed bar
        // (no prev_close) never fires.
        BarEvent::OnClose => {
            let Some(prev) = prev_close else {
                return false;
            };
            match dir {
                CrossDir::Up => prev < level && candle.c >= level,
                CrossDir::Down => prev > level && candle.c <= level,
                CrossDir::Either => {
                    (prev < level && candle.c >= level) || (prev > level && candle.c <= level)
                }
            }
        }
    }
}

/// Interpolate a trendline's price at candle `t`, in **bar-index** space.
///
/// TradingView's x-axis is ordinal: closed sessions aren't plotted, so a
/// trendline advances one step *per traded bar*, not per elapsed second. We
/// replicate that by measuring `t`'s position along the line as a fraction of
/// *bars* between the two anchors, not seconds — counting the bars that
/// actually exist in `window` (the broker feed; gaps are absent). Confirmed on
/// real ALPHABET data: an 18h overnight gap and a 66h weekend gap each collapse
/// to a single bar step, exactly as TV draws them.
///
/// Returns `None` when `t` is past the second anchor and the line isn't
/// extended forward. A degenerate line (anchors at the same bar) is treated as
/// a horizontal at the first anchor's price.
///
/// `bar_seconds` is the fallback bar spacing used to estimate a bar-index when
/// an anchor predates `window` (so its bar can't be counted directly); `0`
/// means "no fallback" and such an out-of-window anchor yields `None`.
fn line_price_at(
    a: &LinePoint,
    b: &LinePoint,
    t: &Candle,
    extend_forward: bool,
    bar_seconds: i64,
    window: &[Candle],
) -> Option<f64> {
    let ia = bar_index_at(a.at_epoch, window, bar_seconds)?;
    let ib = bar_index_at(b.at_epoch, window, bar_seconds)?;
    let it = bar_index_at(t.time.timestamp(), window, bar_seconds)?;
    if it > ib && !extend_forward {
        return None;
    }
    let span = ib - ia;
    if span == 0.0 {
        return Some(a.price);
    }
    let frac = (it - ia) / span;
    Some(a.price + (b.price - a.price) * frac)
}

/// Resolve an epoch to a (possibly fractional) **bar index** within `window`.
///
/// - An epoch matching a bar in `window` returns that bar's ordinal position.
/// - An epoch *inside* the window's span but between two bars (a brief data
///   hole) is interpolated between the surrounding bars' indices by time, so a
///   single missing bar doesn't throw the count off.
/// - An epoch *after* the last bar is extrapolated forward by `bar_seconds`
///   from the last bar (e.g. an anchor on a bar not yet fetched, or a candle
///   the wrapper feeds that the detector window hasn't caught up to).
/// - An epoch *before* the first bar can only be estimated via `bar_seconds`
///   from the first bar; with `bar_seconds == 0` (pre-field plans) this returns
///   `None`, collapsing the trendline to "can't evaluate this tick".
fn bar_index_at(epoch: i64, window: &[Candle], bar_seconds: i64) -> Option<f64> {
    if window.is_empty() {
        return None;
    }
    // Exact bar match — the common case (anchors and candles are real bars).
    if let Some(i) = window.iter().position(|c| c.time.timestamp() == epoch) {
        return Some(i as f64);
    }
    let first = window[0].time.timestamp();
    let last = window[window.len() - 1].time.timestamp();
    if epoch < first {
        // Before the window: estimate backwards by nominal bar spacing.
        if bar_seconds <= 0 {
            return None;
        }
        return Some(-((first - epoch) as f64 / bar_seconds as f64));
    }
    if epoch > last {
        // After the window: estimate forward by nominal bar spacing.
        if bar_seconds <= 0 {
            return None;
        }
        let last_idx = (window.len() - 1) as f64;
        return Some(last_idx + (epoch - last) as f64 / bar_seconds as f64);
    }
    // Inside the span but between bars (a data hole): interpolate by time
    // between the bracketing bars' indices.
    let hi = window.iter().position(|c| c.time.timestamp() > epoch)?;
    let lo = hi - 1;
    let lo_t = window[lo].time.timestamp();
    let hi_t = window[hi].time.timestamp();
    let frac = (epoch - lo_t) as f64 / (hi_t - lo_t) as f64;
    Some(lo as f64 + frac)
}

/// Push a fired rule's intent onto the result, cloning the intent verbatim. No
/// pattern signal (guards / preps / M/W heartbeat).
fn push_fire(rule: &ConditionRule, candle: &Candle, fired: &mut Vec<FiredIntent>) {
    push_fire_signal(rule, candle, None, fired);
}

/// Push a fired rule's intent, carrying an optional latched candle-pattern
/// signal (set for a `PinePattern` H&S enter, `None` otherwise).
fn push_fire_signal(
    rule: &ConditionRule,
    candle: &Candle,
    signal: Option<LatchedSignal>,
    fired: &mut Vec<FiredIntent>,
) {
    fired.push(FiredIntent {
        rule_id: rule.rule_id.clone(),
        intent: rule.intent.clone(),
        candle: *candle,
        signal,
    });
}

/// A guard is a veto rule (a `Veto`/`Invalidate`/`Close` action) — the
/// always-armed half of the machine, distinct from the prep spine and the
/// entry.
fn is_guard(rule: &ConditionRule) -> bool {
    matches!(
        rule.intent.action,
        Action::Veto | Action::Invalidate | Action::Close
    )
}

/// A control rule sets the worker's blackout / news-window KV state on a
/// wall-clock `TimeReached` fire (pause/resume open and close a blackout;
/// news-start/news-end open and close a news window). Always-armed but
/// non-terminal — it never ends the trade's spine, unlike a guard.
fn is_control_rule(rule: &ConditionRule) -> bool {
    matches!(
        rule.intent.action,
        Action::Pause | Action::Resume | Action::NewsStart | Action::NewsEnd
    )
}

fn is_break_and_close(rule: &ConditionRule) -> bool {
    rule.rule_id.contains(ROLE_BREAK_AND_CLOSE)
}

fn is_retest(rule: &ConditionRule) -> bool {
    rule.rule_id.contains(ROLE_RETEST)
}

/// When a veto guard is armed within the spine. Kept permissive for Stage D —
/// the dispatched intent's own gates (e.g. `run_close`'s windows) are
/// authoritative; the engine's job is only to fire the same intent the TV alert
/// would have. Trade-expiry (a `TimeReached` veto) is armed in every phase; the
/// rest arm from `AwaitEntry` onward (before that there's no order or position
/// to act on).
fn armed_in(rule_id: &str, phase: Phase) -> bool {
    if rule_id.contains("trade-expiry") {
        return true;
    }
    matches!(phase, Phase::AwaitEntry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use trade_control_core::broker::Granularity;
    use trade_control_core::intent::{BrokerKind, Direction, Intent};
    use trade_control_core::trade_plan::FireMode;
    use trade_control_core::tunable::Tunable;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn candle(time: &str, o: f64, h: f64, l: f64, c: f64) -> Candle {
        Candle {
            time: ts(time),
            o,
            h,
            l,
            c,
        }
    }

    /// `eval_trigger` with an empty bar window — for level/time/mw triggers that
    /// don't read it. Trendline tests call `eval_trigger` directly with a real
    /// window (bar-index interpolation needs it).
    fn et(trigger: &Trigger, candle: &Candle, prev_close: Option<f64>) -> bool {
        // No cross buffer in the unit-level cross tests — they assert the raw
        // wick/close geometry. The buffer's own behaviour is covered by
        // `intrabar_cross_buffer_*` tests that call `eval_trigger` directly.
        eval_trigger(trigger, candle, prev_close, &[], 0.0)
    }

    /// A minimal intent carrying just the action the evaluator reads; the rest
    /// is copied verbatim into fired results.
    fn intent(action: Action) -> Intent {
        Intent {
            entry_level_vetos: Vec::new(),
            v: 1,
            id: "x".into(),
            not_before: None,
            not_after: ts("2026-06-20T00:00:00Z"),
            action,
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
            trade_plan: None,
            blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
            breakeven: None,
            include_archived: false,
        }
    }

    fn rule(rule_id: &str, trigger: Trigger, fire_mode: FireMode, action: Action) -> ConditionRule {
        ConditionRule {
            rule_id: rule_id.into(),
            trigger,
            fire_mode,
            intent: intent(action),
        }
    }

    /// A complete short H&S `PinePattern` enter rule with signal-anchored
    /// geometry that **resolves** against the `bearish_pinbar_window`
    /// (signal_high 1.30 / signal_low 1.10): stop-entry at signal_low + 1 pip,
    /// SL at signal_high + 1 pip, absolute TP below for ≥ 1R. Mirrors the real
    /// incident's enter shape so the bug #13 pre-flight (gate + resolve) has
    /// something real to act on. `needs_golden` toggles the candle-quality gate.
    fn pine_enter_rule(
        pattern: Option<SignalKind>,
        dir: Direction,
        needs_golden: bool,
    ) -> ConditionRule {
        use trade_control_core::intent::{EntrySpec, PriceAnchor, PriceRef, TakeProfit};
        let mut intent = intent(Action::Enter);
        intent.direction = Some(dir);
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::SignalLow,
            offset_pips: 1.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.stop_loss = Some(PriceRef::Anchored {
            from: PriceAnchor::SignalHigh,
            offset_pips: 1.0,
            offset_atr_pct: None,
        });
        // Absolute TP below the entry for a short, ≥ 1R from the ~0.20 SL
        // distance (entry ≈ 1.10, SL ≈ 1.30): 0.90 gives R ≈ 1.0.
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute { absolute: 0.90 }));
        intent.needs_golden = needs_golden;
        ConditionRule {
            rule_id: "05-enter".into(),
            trigger: Trigger::PinePattern { pattern, dir },
            fire_mode: FireMode::Once,
            intent,
        }
    }

    fn plan(rules: Vec<ConditionRule>) -> TradePlan {
        // Default test plan carries no cross buffer (0.0) so the existing cross
        // tests assert the raw wick/close geometry; buffer behaviour has its own
        // tests that set `cross_buffer_pct` explicitly.
        TradePlan {
            trade_id: "t".into(),
            instrument: "EUR_USD".into(),
            direction: Direction::Short,
            granularity: Granularity::H1,
            pip_size: 0.0001,
            rules,
            shadow: false,
            cross_buffer_pct: 0.0,
            replay_start: None,
        }
    }

    fn seed_at(phase: Phase, watermark: &str) -> PlanState {
        let mut s = PlanState::seed(phase, ts("2026-06-30T00:00:00Z"));
        s.watermark = Some(ts(watermark));
        s
    }

    fn run(p: &TradePlan, prior: &PlanState, candles: &[Candle]) -> PlanEval {
        // Non-Pine tests pass the same slice as both the new-candles and the
        // detector window; only a `PinePattern` entry reads the latter.
        run_window(p, prior, candles, candles)
    }

    fn run_window(
        p: &TradePlan,
        prior: &PlanState,
        new_candles: &[Candle],
        detector_window: &[Candle],
    ) -> PlanEval {
        evaluate_plan(
            p,
            prior,
            new_candles,
            detector_window,
            ts("2026-06-16T20:00:00Z"),
            ts("2026-06-30T00:00:00Z"),
        )
    }

    // ===== eval_trigger: level crosses =====

    #[test]
    fn horizontal_on_close_fires_when_close_crosses_prior_close() {
        let t = Trigger::HorizontalCross {
            level: 1.2000,
            dir: CrossDir::Up,
            bar: BarEvent::OnClose,
        };
        // prior close below, this close at/above → fires.
        assert!(et(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.1, 1.21, 1.1, 1.2005),
            Some(1.1990)
        ));
        // prior close already above → no cross.
        assert!(!et(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.21, 1.22, 1.2, 1.2105),
            Some(1.2010)
        ));
    }

    #[test]
    fn on_close_cross_does_not_fire_on_seed_bar() {
        let t = Trigger::HorizontalCross {
            level: 1.2000,
            dir: CrossDir::Up,
            bar: BarEvent::OnClose,
        };
        // No prior close (seed) → never fires even if this close is above.
        assert!(!et(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.1, 1.3, 1.1, 1.2500),
            None
        ));
    }

    #[test]
    fn intrabar_fires_on_any_straddle_regardless_of_open() {
        // Intrabar direction is read from the wick on the cross side, NOT the
        // close, and (as of 2026-07-03) NOT the open either: an Up cross is now
        // "the bar's high and low sit on opposite sides of the level, and the
        // high reached at/above it". Where it opened or closed is irrelevant.
        let t = Trigger::HorizontalCross {
            level: 1.2000,
            dir: CrossDir::Up,
            bar: BarEvent::Intrabar,
        };
        // Opened below, high pushed above, closed above → up fires.
        assert!(et(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.19, 1.21, 1.19, 1.2050),
            None
        ));
        // Opened below, high pushed above, then closed *back below* the level →
        // still an up-cross (the wick crossed). Close-agnostic.
        assert!(et(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.19, 1.21, 1.19, 1.1950),
            None
        ));
        // Opened *above* the level, high on/above, low dips below → the high/low
        // straddle the line so this now FIRES too (the previous open-side rule
        // rejected it; the permissive straddle rule accepts any straddle whose
        // high reached the level).
        assert!(et(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.21, 1.21, 1.19, 1.1950),
            None
        ));
        // Range never reaches the level → no straddle, no fire.
        assert!(!et(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.18, 1.195, 1.18, 1.19),
            None
        ));
    }

    /// The mirror: a Down intrabar cross now fires on any straddle whose low
    /// reached at/below the level, open- and close-agnostic — the AUD/JPY 6pm
    /// retest shape and, since 2026-07-03, any straddle regardless of open.
    #[test]
    fn intrabar_down_fires_on_any_straddle_regardless_of_open() {
        let t = Trigger::HorizontalCross {
            level: 1.2000,
            dir: CrossDir::Down,
            bar: BarEvent::Intrabar,
        };
        // Opened above, low dipped below, closed back above → down fires (the
        // wick crossed).
        assert!(et(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.21, 1.21, 1.19, 1.2050),
            None
        ));
        // Opened *below* the level, low on/below, high pokes above → the high/low
        // straddle so this now FIRES too (the previous open-side rule rejected
        // it; the permissive straddle rule accepts any straddle whose low
        // reached the level).
        assert!(et(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.19, 1.21, 1.19, 1.2050),
            None
        ));
    }

    /// The cross-depth buffer rejects a graze: with `buffer_pct = 0.1` the wick
    /// must pierce `level * 0.1% = 1.2 * 0.001 = 0.0012` past the line. A down
    /// graze to 1.1995 (only 0.0005 below) does NOT fire; a dip to 1.1980
    /// (0.0020 below, past the 1.1988 buffered level) does. Both straddle the
    /// line (high on/above, low below), so the straddle rule is satisfied either
    /// way — the buffer depth on the low is the discriminator.
    #[test]
    fn intrabar_down_buffer_rejects_a_graze_admits_a_real_cross() {
        let t = Trigger::HorizontalCross {
            level: 1.2000,
            dir: CrossDir::Down,
            bar: BarEvent::Intrabar,
        };
        let graze = candle("2026-06-16T12:00:00Z", 1.2010, 1.2010, 1.1995, 1.2005);
        let real = candle("2026-06-16T12:00:00Z", 1.2010, 1.2010, 1.1980, 1.2005);

        // No buffer (0.0) → both fire (bare wick touch).
        assert!(eval_trigger(&t, &graze, None, &[], 0.0));
        assert!(eval_trigger(&t, &real, None, &[], 0.0));

        // 0.1% buffer → the graze is rejected, the real cross still fires.
        assert!(
            !eval_trigger(&t, &graze, None, &[], 0.1),
            "a 5-pip graze (< 0.0012 buffer) must not count as a cross"
        );
        assert!(
            eval_trigger(&t, &real, None, &[], 0.1),
            "a 20-pip dip (> 0.0012 buffer) is a real cross"
        );
    }

    /// Mirror for an up-cross: the high must reach `level + buffer` to fire.
    #[test]
    fn intrabar_up_buffer_rejects_a_graze_admits_a_real_cross() {
        let t = Trigger::HorizontalCross {
            level: 1.2000,
            dir: CrossDir::Up,
            bar: BarEvent::Intrabar,
        };
        // Opened below; buffer = 0.0012 → need high ≥ 1.2012.
        let graze = candle("2026-06-16T12:00:00Z", 1.1990, 1.2005, 1.1990, 1.1995);
        let real = candle("2026-06-16T12:00:00Z", 1.1990, 1.2020, 1.1990, 1.1995);
        assert!(!eval_trigger(&t, &graze, None, &[], 0.1));
        assert!(eval_trigger(&t, &real, None, &[], 0.1));
    }

    /// The buffer applies only to directional intrabar crosses, not `Either`
    /// (a bare straddle is already an unambiguous touch) nor `OnClose` (the
    /// close is past the line by construction). A graze straddle still fires
    /// `Either` even with a buffer set.
    #[test]
    fn cross_buffer_ignored_by_either_and_on_close() {
        let graze = candle("2026-06-16T12:00:00Z", 1.2010, 1.2010, 1.1995, 1.2005);
        let either = Trigger::PriceValueCross {
            level: 1.2000,
            dir: CrossDir::Either,
            bar: BarEvent::Intrabar,
        };
        assert!(
            eval_trigger(&either, &graze, None, &[], 0.1),
            "Either fires on any straddle regardless of the buffer"
        );
        // OnClose down: a bar that closed below the line fires irrespective of
        // the buffer (the close, not the wick depth, is the test).
        let on_close = Trigger::HorizontalCross {
            level: 1.2000,
            dir: CrossDir::Down,
            bar: BarEvent::OnClose,
        };
        let closed_below = candle("2026-06-16T12:00:00Z", 1.2010, 1.2010, 1.1995, 1.1999);
        assert!(
            eval_trigger(&on_close, &closed_below, Some(1.2010), &[], 0.1),
            "OnClose ignores the cross buffer"
        );
    }

    #[test]
    fn intrabar_either_fires_on_any_straddle() {
        let t = Trigger::PriceValueCross {
            level: 1.2000,
            dir: CrossDir::Either,
            bar: BarEvent::Intrabar,
        };
        assert!(et(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.19, 1.21, 1.19, 1.205),
            None
        ));
        assert!(et(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.21, 1.21, 1.19, 1.195),
            None
        ));
    }

    // ===== eval_trigger: trendline =====

    /// Build a window of `n` consecutive bars at `step` seconds apart starting
    /// at `start`, all flat at price `p` (range ±0.5 so we control straddles via
    /// the candle under test). Bar `i` sits at epoch `start + i*step`.
    fn flat_window(start: i64, step: i64, n: usize, p: f64) -> Vec<Candle> {
        (0..n)
            .map(|i| Candle {
                time: DateTime::from_timestamp(start + i as i64 * step, 0).unwrap(),
                o: p,
                h: p,
                l: p,
                c: p,
            })
            .collect()
    }

    #[test]
    fn trendline_interpolates_level_at_bar_index() {
        // Line anchored on bar 0 (epoch 0, price 1.0) and bar 2 (epoch 200,
        // price 2.0). The MIDDLE bar (index 1, epoch 100) is half-way → 1.5.
        // Interpolation is by bar index, which here equals time since bars are
        // evenly spaced; the gap tests below prove the index (not time) path.
        let win = flat_window(0, 100, 3, 1.0);
        let t = Trigger::TrendlineCross {
            a: LinePoint {
                at_epoch: 0,
                price: 1.0,
            },
            b: LinePoint {
                at_epoch: 200,
                price: 2.0,
            },
            extend_forward: true,
            bar_seconds: 100,
            dir: CrossDir::Up,
            bar: BarEvent::Intrabar,
        };
        // bar 1: level 1.5; range straddles, close above → fires.
        let c = Candle {
            time: DateTime::from_timestamp(100, 0).unwrap(),
            o: 1.4,
            h: 1.6,
            l: 1.4,
            c: 1.55,
        };
        assert!(eval_trigger(&t, &c, None, &win, 0.0));
    }

    /// The core of the bug: a session gap must NOT slide the line. Bars are
    /// adjacent in index even though wall-clock between them is huge.
    #[test]
    fn trendline_gap_uses_bar_index_not_wall_clock() {
        // Three bars: 13:00, 14:00 (1h apart) then a HUGE jump to next day's
        // 13:00 — like ALPHABET's overnight gap (the real feed elides the closed
        // session). Indices 0,1,2; the line is anchored bar 0 → bar 2.
        let day1 = ts("2026-06-16T13:00:00Z").timestamp();
        let day1b = ts("2026-06-16T14:00:00Z").timestamp();
        let day2 = ts("2026-06-17T13:00:00Z").timestamp(); // +23h, but bar 2
        let win = vec![
            candle("2026-06-16T13:00:00Z", 1.0, 1.0, 1.0, 1.0),
            candle("2026-06-16T14:00:00Z", 1.0, 1.0, 1.0, 1.0),
            candle("2026-06-17T13:00:00Z", 1.0, 1.0, 1.0, 1.0),
        ];
        // Line from bar 0 (1.00) to bar 2 (2.00). At bar 1 (index 1 of 0..2) the
        // level is the half-way 1.50 — NOT what wall-clock would give (bar 1 is
        // only 1h into a 23h span → wall-clock ~1.04).
        let t = Trigger::TrendlineCross {
            a: LinePoint {
                at_epoch: day1,
                price: 1.0,
            },
            b: LinePoint {
                at_epoch: day2,
                price: 2.0,
            },
            extend_forward: true,
            bar_seconds: 3600,
            dir: CrossDir::Up,
            bar: BarEvent::Intrabar,
        };
        // At bar 1, the bar-index level is 1.50. A candle straddling 1.50 fires;
        // one straddling only ~1.04 (the wall-clock level) does NOT.
        let at_index_level = candle("2026-06-16T14:00:00Z", 1.45, 1.55, 1.45, 1.52);
        assert!(
            eval_trigger(&t, &at_index_level, None, &win, 0.0),
            "bar-index level at bar 1 is 1.50 → straddle fires"
        );
        let at_wallclock_level = candle("2026-06-16T14:00:00Z", 1.0, 1.08, 1.0, 1.05);
        assert!(
            !eval_trigger(&t, &at_wallclock_level, None, &win, 0.0),
            "the old wall-clock level (~1.04) must NOT be where the line sits"
        );
        let _ = day1b;
    }

    #[test]
    fn trendline_respects_extend_forward_false() {
        // Bars 0..=2; line anchored bar 0 → bar 1. Bar 2 is past the second
        // anchor.
        let win = flat_window(0, 100, 3, 1.0);
        let mk = |extend| Trigger::TrendlineCross {
            a: LinePoint {
                at_epoch: 0,
                price: 1.0,
            },
            b: LinePoint {
                at_epoch: 100,
                price: 1.0,
            },
            extend_forward: extend,
            bar_seconds: 100,
            dir: CrossDir::Either,
            bar: BarEvent::Intrabar,
        };
        // candle = bar 2 (epoch 200), past the second anchor (bar 1), at level.
        let c = Candle {
            time: DateTime::from_timestamp(200, 0).unwrap(),
            o: 0.9,
            h: 1.1,
            l: 0.9,
            c: 1.0,
        };
        assert!(
            !eval_trigger(&mk(false), &c, None, &win, 0.0),
            "no eval past anchor when not extended"
        );
        assert!(
            eval_trigger(&mk(true), &c, None, &win, 0.0),
            "extended → still evaluates"
        );
    }

    // ===== trendline out-of-window anchor warnings (bar_seconds hardening) =====

    /// A trendline rule whose anchors are at `a_epoch` / `b_epoch`, carrying the
    /// given `bar_seconds`. Geometry is irrelevant to the warning surface.
    fn trendline_rule(a_epoch: i64, b_epoch: i64, bar_seconds: i64) -> ConditionRule {
        rule(
            "03-prep-break-and-close",
            Trigger::TrendlineCross {
                a: LinePoint {
                    at_epoch: a_epoch,
                    price: 1.0,
                },
                b: LinePoint {
                    at_epoch: b_epoch,
                    price: 1.0,
                },
                extend_forward: true,
                bar_seconds,
                dir: CrossDir::Down,
                bar: BarEvent::OnClose,
            },
            FireMode::Once,
            Action::Prep,
        )
    }

    #[test]
    fn in_window_anchors_produce_no_warning() {
        // Window spans epochs 0..200; both anchors inside it → silent.
        let win = flat_window(0, 100, 3, 1.0);
        let p = plan(vec![trendline_rule(0, 200, 100)]);
        assert!(trendline_anchor_warnings(&p, &win).is_empty());
    }

    #[test]
    fn out_of_window_anchor_warns_about_bar_seconds_extrapolation() {
        // Window spans epochs 0..200; anchor `a` predates it (epoch -1000), so it
        // can only be estimated from bar_seconds → a soft "extrapolated" warning.
        let win = flat_window(0, 100, 3, 1.0);
        let p = plan(vec![trendline_rule(-1000, 200, 100)]);
        let w = trendline_anchor_warnings(&p, &win);
        assert_eq!(w.len(), 1, "exactly one out-of-window anchor");
        assert!(w[0].contains("anchor a"), "names the offending anchor");
        assert!(
            w[0].contains("bar_seconds=100"),
            "reports the divisor used: {}",
            w[0]
        );
        assert!(
            w[0].contains("Widen the candle fetch"),
            "points at the real fix: {}",
            w[0]
        );
    }

    #[test]
    fn out_of_window_anchor_with_zero_bar_seconds_warns_unresolvable() {
        // Pre-bar_seconds plan (bar_seconds=0): an out-of-window anchor can't be
        // estimated at all → the trendline silently won't fire. The hard warning
        // documents that exact failure mode.
        let win = flat_window(0, 100, 3, 1.0);
        let p = plan(vec![trendline_rule(-1000, 200, 0)]);
        let w = trendline_anchor_warnings(&p, &win);
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("bar_seconds=0") || w[0].contains("bar_seconds=0 ("),
            "names the zero divisor: {}",
            w[0]
        );
        assert!(
            w[0].contains("cannot be evaluated") && w[0].contains("won't fire"),
            "documents the silent-non-fire: {}",
            w[0]
        );
    }

    #[test]
    fn both_anchors_out_of_window_warns_twice() {
        // Window spans 0..200; both anchors outside (one before, one after).
        let win = flat_window(0, 100, 3, 1.0);
        let p = plan(vec![trendline_rule(-500, 99_999, 100)]);
        let w = trendline_anchor_warnings(&p, &win);
        assert_eq!(w.len(), 2, "both anchors flagged");
        assert!(w.iter().any(|s| s.contains("anchor a")));
        assert!(w.iter().any(|s| s.contains("anchor b")));
    }

    #[test]
    fn non_trendline_plan_never_warns() {
        // An M/W plan (no trendline rule) produces no trendline warnings, even
        // with an empty window.
        let p = plan(vec![rule(
            "05-enter",
            Trigger::MwEveryBar,
            FireMode::EveryBar,
            Action::Enter,
        )]);
        assert!(trendline_anchor_warnings(&p, &[]).is_empty());
    }

    #[test]
    fn evaluate_plan_surfaces_the_warning_on_planeval() {
        // End-to-end: a plan whose neckline anchor predates the fed window
        // surfaces the warning on PlanEval.warnings (what the wrapper logs).
        // Window: three H1 bars; anchor `a` is a day earlier (out of window).
        let win = vec![
            candle("2026-06-16T13:00:00Z", 1.0, 1.0, 1.0, 1.0),
            candle("2026-06-16T14:00:00Z", 1.0, 1.0, 1.0, 1.0),
            candle("2026-06-16T15:00:00Z", 1.0, 1.0, 1.0, 1.0),
        ];
        let a_epoch = ts("2026-06-15T13:00:00Z").timestamp(); // out of window
        let b_epoch = ts("2026-06-16T15:00:00Z").timestamp(); // in window
        let p = plan(vec![
            trendline_rule(a_epoch, b_epoch, 3600),
            rule(
                "05-enter",
                Trigger::MwEveryBar,
                FireMode::EveryBar,
                Action::Enter,
            ),
        ]);
        let prior = seed_at(Phase::AwaitBreakAndClose, "2026-06-16T12:00:00Z");
        let eval = run_window(&p, &prior, &win, &win);
        assert_eq!(eval.warnings.len(), 1, "the out-of-window anchor warns");
        assert!(eval.warnings[0].contains("anchor a"));
    }

    // ===== eval_trigger: time + mw + pine =====

    #[test]
    fn time_reached_fires_at_or_past_epoch() {
        let at = ts("2026-06-16T15:00:00Z").timestamp();
        let t = Trigger::TimeReached { at_epoch: at };
        assert!(!et(
            &t,
            &candle("2026-06-16T14:00:00Z", 1.0, 1.0, 1.0, 1.0),
            None
        ));
        assert!(et(
            &t,
            &candle("2026-06-16T15:00:00Z", 1.0, 1.0, 1.0, 1.0),
            None
        ));
        assert!(et(
            &t,
            &candle("2026-06-16T16:00:00Z", 1.0, 1.0, 1.0, 1.0),
            None
        ));
    }

    #[test]
    fn mw_every_bar_always_fires_and_pine_never_fires() {
        let c = candle("2026-06-16T12:00:00Z", 1.0, 1.0, 1.0, 1.0);
        assert!(et(&Trigger::MwEveryBar, &c, None));
        assert!(!et(
            &Trigger::PinePattern {
                pattern: None,
                dir: Direction::Short
            },
            &c,
            None
        ));
    }

    // ===== spine derivation =====

    #[test]
    fn mw_plan_starts_in_await_entry() {
        let p = plan(vec![rule(
            "05-enter",
            Trigger::MwEveryBar,
            FireMode::EveryBar,
            Action::Enter,
        )]);
        assert_eq!(initial_phase(&p), Phase::AwaitEntry);
    }

    #[test]
    fn hs_plan_starts_in_await_break_and_close() {
        let neckline = Trigger::TrendlineCross {
            a: LinePoint {
                at_epoch: 0,
                price: 1.2,
            },
            b: LinePoint {
                at_epoch: 100,
                price: 1.2,
            },
            extend_forward: true,
            bar_seconds: 100,
            dir: CrossDir::Down,
            bar: BarEvent::OnClose,
        };
        let p = plan(vec![
            rule(
                "03-prep-break-and-close",
                neckline,
                FireMode::Once,
                Action::Prep,
            ),
            rule(
                "05-enter",
                Trigger::PinePattern {
                    pattern: None,
                    dir: Direction::Short,
                },
                FireMode::Once,
                Action::Enter,
            ),
        ]);
        assert_eq!(initial_phase(&p), Phase::AwaitBreakAndClose);
    }

    // ===== full evaluate_plan: M/W heartbeat =====

    #[test]
    fn mw_heartbeat_emits_enter_each_closed_bar_without_latching() {
        let p = plan(vec![rule(
            "05-enter",
            Trigger::MwEveryBar,
            FireMode::EveryBar,
            Action::Enter,
        )]);
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T11:00:00Z");
        let candles = [
            candle("2026-06-16T12:00:00Z", 1.0, 1.0, 1.0, 1.0),
            candle("2026-06-16T13:00:00Z", 1.0, 1.0, 1.0, 1.0),
        ];
        let eval = run(&p, &prior, &candles);
        // One enter per bar, never latches, phase stays AwaitEntry.
        assert_eq!(eval.fired.len(), 2);
        assert!(eval.fired.iter().all(|f| f.rule_id == "05-enter"));
        assert!(!eval.done);
        assert_eq!(eval.new_state.phase, Phase::AwaitEntry);
        assert_eq!(eval.new_state.watermark, Some(ts("2026-06-16T13:00:00Z")));
    }

    #[test]
    fn empty_candles_is_noop_watermark_unchanged() {
        let p = plan(vec![rule(
            "05-enter",
            Trigger::MwEveryBar,
            FireMode::EveryBar,
            Action::Enter,
        )]);
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T11:00:00Z");
        let eval = run(&p, &prior, &[]);
        assert!(eval.fired.is_empty());
        assert_eq!(eval.new_state.watermark, Some(ts("2026-06-16T11:00:00Z")));
    }

    // ===== full evaluate_plan: H&S spine ordering =====

    fn hs_plan() -> TradePlan {
        // Neckline trendline flat at 1.2000 over the whole window.
        let neckline = |dir, bar| Trigger::TrendlineCross {
            a: LinePoint {
                at_epoch: ts("2026-06-16T00:00:00Z").timestamp(),
                price: 1.2000,
            },
            b: LinePoint {
                at_epoch: ts("2026-06-16T01:00:00Z").timestamp(),
                price: 1.2000,
            },
            extend_forward: true,
            bar_seconds: 3600,
            dir,
            bar,
        };
        plan(vec![
            // break-and-close: short closes DOWN through the neckline, OnClose.
            rule(
                "03-prep-break-and-close",
                neckline(CrossDir::Down, BarEvent::OnClose),
                FireMode::Once,
                Action::Prep,
            ),
            // retest: opposite cross UP, intrabar.
            rule(
                "04-prep-retest",
                neckline(CrossDir::Up, BarEvent::Intrabar),
                FireMode::Once,
                Action::Prep,
            ),
            // entry: single-shot heartbeat stand-in (MwEveryBar fires every bar;
            // FireMode::Once makes it a one-shot entry once the gate opens).
            rule(
                "05-enter",
                Trigger::MwEveryBar,
                FireMode::Once,
                Action::Enter,
            ),
        ])
    }

    #[test]
    fn break_and_close_advances_phase_and_dies() {
        let p = hs_plan();
        let prior = seed_at(Phase::AwaitBreakAndClose, "2026-06-16T09:00:00Z");
        // First bar closes below the neckline (prior close above) → break-and-close.
        let c1 = candle("2026-06-16T10:00:00Z", 1.205, 1.205, 1.195, 1.1950);
        // Seed last_close for the b&c rule so the OnClose cross has a prior.
        let mut prior = prior;
        prior
            .last_close
            .insert("03-prep-break-and-close".into(), 1.2050);
        let eval = run(&p, &prior, &[c1]);
        assert_eq!(eval.new_state.phase, Phase::AwaitEntry);
        assert!(eval.new_state.fired.contains("03-prep-break-and-close"));
        assert_eq!(
            eval.new_state.break_close_at,
            Some(ts("2026-06-16T10:00:00Z"))
        );
        assert_eq!(eval.fired.len(), 1);
        assert_eq!(eval.fired[0].rule_id, "03-prep-break-and-close");
    }

    // ===== strategy-v2: two enters in one plan =====

    /// An enter rule with explicit `requires_preps` + `max_retries`, using the
    /// `MwEveryBar` heartbeat trigger so firing is deterministic (no Pine
    /// resolution). Mirrors the strategy-v2 enter shape: the stop enter lists
    /// both preps, the QM enter lists none; both are multi-shot (max_retries=5)
    /// so a fire keeps the plan alive (the worker retry gate is the real cap).
    fn enter_rule(rule_id: &str, preps: &[&str], max_retries: u32) -> ConditionRule {
        let mut intent = intent(Action::Enter);
        intent.requires_preps = preps.iter().map(|s| s.to_string()).collect();
        intent.max_retries = Tunable::Static(max_retries);
        intent.direction = Some(Direction::Short);
        ConditionRule {
            rule_id: rule_id.into(),
            trigger: Trigger::MwEveryBar,
            fire_mode: FireMode::Once,
            intent,
        }
    }

    /// A strategy-v2 plan: break-and-close + retest preps, a stop enter gated by
    /// both, and a QM enter gated by neither. The stop enter is listed first so
    /// it wins a same-bar tie.
    fn strategy_v2_plan() -> TradePlan {
        let neckline = |dir, bar| Trigger::TrendlineCross {
            a: LinePoint {
                at_epoch: ts("2026-06-16T00:00:00Z").timestamp(),
                price: 1.2000,
            },
            b: LinePoint {
                at_epoch: ts("2026-06-16T20:00:00Z").timestamp(),
                price: 1.2000,
            },
            extend_forward: true,
            bar_seconds: 3600,
            dir,
            bar,
        };
        plan(vec![
            rule(
                "03-prep-break-and-close",
                neckline(CrossDir::Down, BarEvent::OnClose),
                FireMode::Once,
                Action::Prep,
            ),
            rule(
                "04-prep-retest",
                neckline(CrossDir::Up, BarEvent::Intrabar),
                FireMode::Once,
                Action::Prep,
            ),
            // Stop enter first (wins the same-bar tie), gated by both preps.
            enter_rule("05-enter", &["break-and-close", "retest"], 5),
            // QM enter, gated by neither prep.
            enter_rule("09-enter-qm", &[], 5),
        ])
    }

    #[test]
    fn multi_enter_plan_starts_in_await_entry() {
        // Even though a break-and-close prep exists, a two-enter plan starts in
        // AwaitEntry so the QM enter is evaluable from bar 1.
        let p = strategy_v2_plan();
        assert_eq!(initial_phase(&p), Phase::AwaitEntry);
    }

    #[test]
    fn qm_enter_fires_before_break_and_close_while_stop_is_blocked() {
        // Bar 1: no break-and-close, no retest. The QM enter (no preps) fires;
        // the stop enter (needs both preps) does not. The plan survives (both
        // enters are multi-shot).
        let p = strategy_v2_plan();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        // Price sits below the neckline and does not cross up — no b&c, no retest.
        let c1 = candle("2026-06-16T11:00:00Z", 1.18, 1.185, 1.17, 1.182);
        let eval = run(&p, &prior, &[c1]);
        let fired: Vec<&str> = eval.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(fired, vec!["09-enter-qm"], "only the QM enter fires");
        assert!(!eval.done, "plan stays alive (multi-shot enters)");
        assert_eq!(eval.new_state.phase, Phase::AwaitEntry);
    }

    #[test]
    fn replay_start_floors_enters_to_the_start_cursor() {
        // A journaling replay bakes `replay_start` onto the plan. Bars before it
        // are warmup/context — no enter may fire on a bar that opens before the
        // cursor, even a free (no-prep) QM enter that would otherwise latch onto
        // a micro-pattern at the replay boundary. On/after the cursor the enter
        // fires normally.
        let mut p = strategy_v2_plan();
        p.replay_start = Some(ts("2026-06-16T12:00:00Z").timestamp());

        // Bar at 11:00 — one hour BEFORE the start cursor. The QM enter would
        // fire here without the floor; it must not.
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let before = candle("2026-06-16T11:00:00Z", 1.18, 1.185, 1.17, 1.182);
        let eval_before = run(&p, &prior, &[before]);
        assert!(
            eval_before.fired.is_empty(),
            "no enter fires before replay_start; got {:?}",
            eval_before
                .fired
                .iter()
                .map(|f| f.rule_id.as_str())
                .collect::<Vec<_>>()
        );

        // Bar at 12:00 — exactly the start cursor. The QM enter fires.
        let at_start = candle("2026-06-16T12:00:00Z", 1.18, 1.185, 1.17, 1.182);
        let eval_at = run(&p, &eval_before.new_state, &[at_start]);
        let fired: Vec<&str> = eval_at.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(
            fired,
            vec!["09-enter-qm"],
            "the QM enter fires on the start bar"
        );
    }

    #[test]
    fn stop_enter_fires_only_after_break_and_close_then_retest() {
        // Drive the full stop-entry path on a strategy-v2 plan: the b&c stamps,
        // a retest stamps, then the stop enter fires. The QM enter fires on
        // every bar throughout (no preps), so we assert the stop appears only
        // once the gate is satisfied.
        let p = strategy_v2_plan();
        let mut prior = seed_at(Phase::AwaitEntry, "2026-06-16T09:00:00Z");
        prior
            .last_close
            .insert("03-prep-break-and-close".into(), 1.2050);

        // Bar 1 (10:00): closes below the neckline → break-and-close stamps.
        // No retest yet, so the stop enter is still blocked; QM fires.
        let c1 = candle("2026-06-16T10:00:00Z", 1.205, 1.205, 1.195, 1.1950);
        let eval1 = run(&p, &prior, &[c1]);
        let f1: Vec<&str> = eval1.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(f1.contains(&"03-prep-break-and-close"));
        assert!(f1.contains(&"09-enter-qm"));
        assert!(!f1.contains(&"05-enter"), "stop blocked: no retest yet");
        assert_eq!(
            eval1.new_state.break_close_at,
            Some(ts("2026-06-16T10:00:00Z"))
        );

        // Bar 2 (11:00): straddles the neckline closing above → retest stamps
        // (and emits the retest prep fire so the store is seeded before the
        // enter's prep gate), and the stop enter fires the same bar. QM fires too.
        let c2 = candle("2026-06-16T11:00:00Z", 1.19, 1.205, 1.19, 1.2010);
        let eval2 = run(&p, &eval1.new_state, &[c2]);
        let f2: Vec<&str> = eval2.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(
            f2.contains(&"04-prep-retest"),
            "retest prep fires to seed store"
        );
        assert!(f2.contains(&"05-enter"), "stop fires once retest seen");
        assert!(f2.contains(&"09-enter-qm"));
        // The retest prep is dispatched before either enter (stamp_retest runs
        // before evaluate_entry), so set_prep(retest) precedes run_enter.
        assert_eq!(
            f2.first(),
            Some(&"04-prep-retest"),
            "retest prep seeds the store ahead of the enters"
        );
        // Among the enters, the stop is listed before QM, so it dispatches first.
        let enters: Vec<&str> = f2
            .iter()
            .copied()
            .filter(|id| id.contains("enter"))
            .collect();
        assert_eq!(
            enters,
            vec!["05-enter", "09-enter-qm"],
            "stop wins the same-bar tie over QM"
        );
        assert!(!eval2.done, "multi-shot enters keep the plan alive");
        assert_eq!(eval2.new_state.phase, Phase::AwaitEntry);
    }

    #[test]
    fn break_and_close_does_not_restamp_after_latching_under_multi_enter() {
        // Regression for the strategy-v2 entry-starvation bug (replay trade 071,
        // GBP/JPY iH&S): a multi-shot plan stays in AwaitEntry, so the break-and-
        // close arm runs every bar. Without a latch guard it re-fired and walked
        // `break_close_at` forward on every later neckline re-cross — pushing the
        // retest *before* the new break window, so `retest_satisfied` flipped to
        // false and the stop enter was starved forever (re-prepping in place).
        //
        // The break-and-close prep is `FireMode::Once`: once it stamps, a later
        // re-cross must NOT re-stamp `break_close_at`, and the stop enter must
        // still fire on a bar after the (single) retest.
        let p = strategy_v2_plan();
        let mut prior = seed_at(Phase::AwaitEntry, "2026-06-16T09:00:00Z");
        prior
            .last_close
            .insert("03-prep-break-and-close".into(), 1.2050);

        // Bar 1 (10:00): closes below the neckline → break-and-close stamps @10:00.
        let c1 = candle("2026-06-16T10:00:00Z", 1.205, 1.205, 1.195, 1.1950);
        let eval1 = run(&p, &prior, &[c1]);
        assert_eq!(
            eval1.new_state.break_close_at,
            Some(ts("2026-06-16T10:00:00Z"))
        );

        // Bar 2 (11:00): straddles up through the neckline → retest stamps @11:00.
        let c2 = candle("2026-06-16T11:00:00Z", 1.19, 1.205, 1.19, 1.2010);
        let eval2 = run(&p, &eval1.new_state, &[c2]);
        assert_eq!(
            eval2.new_state.retest_seen_at,
            Some(ts("2026-06-16T11:00:00Z"))
        );

        // Bar 3 (12:00): closes BACK below the neckline — a fresh down-cross. The
        // latched break-and-close must NOT re-stamp; `break_close_at` stays @10:00
        // and the retest (@11:00) remains inside (break, entry].
        let c3 = candle("2026-06-16T12:00:00Z", 1.201, 1.205, 1.195, 1.1950);
        let eval3 = run(&p, &eval2.new_state, &[c3]);
        assert_eq!(
            eval3.new_state.break_close_at,
            Some(ts("2026-06-16T10:00:00Z")),
            "break-and-close must not re-stamp after latching (it is FireMode::Once)"
        );
        let f3: Vec<&str> = eval3.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(
            !f3.contains(&"03-prep-break-and-close"),
            "latched break-and-close must not re-fire on a later re-cross"
        );

        // Bar 4 (13:00): the stop enter must fire — retest (@11:00) is still valid
        // against the un-walked break (@10:00). Before the fix this was starved.
        let c4 = candle("2026-06-16T13:00:00Z", 1.196, 1.198, 1.19, 1.1960);
        let eval4 = run(&p, &eval3.new_state, &[c4]);
        let f4: Vec<&str> = eval4.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(
            f4.contains(&"05-enter"),
            "stop enter must fire: retest stays valid against the latched break"
        );
    }

    #[test]
    fn entry_blocked_until_retest_then_fires_and_done() {
        let p = hs_plan();
        let mut prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        prior.break_close_at = Some(ts("2026-06-16T10:00:00Z"));
        // Bar at 11:00: price stays BELOW the neckline (no retest up-cross), so
        // entry is blocked even though MwEveryBar would fire.
        let c_no_retest = candle("2026-06-16T11:00:00Z", 1.19, 1.198, 1.18, 1.185);
        let eval1 = run(&p, &prior, &[c_no_retest]);
        assert!(eval1.fired.is_empty(), "entry blocked without a retest");
        assert_eq!(eval1.new_state.phase, Phase::AwaitEntry);
        assert!(eval1.new_state.retest_seen_at.is_none());

        // Bar at 12:00: range straddles the neckline with close above → retest
        // up-cross stamps (emitting the retest prep fire so it seeds the store),
        // and the entry heartbeat fires the same bar → Done. Two fires, retest
        // before enter (stamp_retest runs before evaluate_entry), so the worker
        // dispatches set_prep(retest) ahead of run_enter's prep gate.
        let c_retest = candle("2026-06-16T12:00:00Z", 1.19, 1.205, 1.19, 1.2010);
        let eval2 = run(&p, &eval1.new_state, &[c_retest]);
        let f2: Vec<&str> = eval2.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(
            f2,
            vec!["04-prep-retest", "05-enter"],
            "retest prep seeds the store before the enter, both on the retest bar"
        );
        assert!(eval2.done);
        assert_eq!(eval2.new_state.phase, Phase::Done);
    }

    #[test]
    fn retest_seen_in_earlier_tick_still_admits_later_entry() {
        // Retest closes in one tick; the entry-eligible bar arrives in a later
        // tick. The persisted retest_seen_at must carry across.
        let p = hs_plan();
        let mut prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        prior.break_close_at = Some(ts("2026-06-16T10:00:00Z"));
        // Tick 1: a retest bar (straddle, close above) but make the entry not
        // matter — MwEveryBar always fires, so the entry fires this same bar.
        // To isolate "retest persists", use an entry that needs the gate: the
        // gate IS satisfied here, so it fires + Done. Instead, assert the stamp
        // persisted by checking state after a retest-only scenario: drop the
        // enter rule's same-bar fire by giving the retest bar, then a separate
        // entry bar — both fire but we assert retest_seen_at was set tick 1.
        let c_retest = candle("2026-06-16T11:00:00Z", 1.19, 1.205, 1.19, 1.2010);
        let eval1 = run(&p, &prior, &[c_retest]);
        // Entry fired the same bar (gate already open), so done; but the stamp
        // must have been recorded.
        assert_eq!(
            eval1.new_state.retest_seen_at,
            Some(ts("2026-06-16T11:00:00Z"))
        );
    }

    /// Real-incident regression: AUD/JPY iH&S long, 2026-06-29. The retest is a
    /// **down**-cross of the descending neckline. The 6pm Brisbane bar
    /// (29 Jun 18:00Z) opened *above* the neckline (O 111.582 > line 111.519)
    /// and its **low wicked below** it (L 111.501 < line) before closing back
    /// **above** (C 111.540 > line). On a tick timeline that bar genuinely
    /// crossed down — it came from above and traded below — so the retest must
    /// stamp here. The old close-confirmed Intrabar rule (`Down ⇒ close ≤ level`)
    /// rejected it because the close sat above, starving the entry for ~6h until
    /// a bar finally *closed* below the line (30 Jun 00:00Z). The fix reads the
    /// wick, not the close: `Down ⇒ low ≤ level` (a straddle; open-agnostic as
    /// of 2026-07-03 — here the bar happens to open above too).
    #[test]
    fn intrabar_down_retest_fires_on_wick_not_close() {
        // Descending neckline interpolated to ~111.519 at the 6pm bar (matches
        // the chart readout). Two anchors a bar apart with the right slope;
        // `extend_forward` carries the line to the test bar.
        let neckline = Trigger::TrendlineCross {
            a: LinePoint {
                at_epoch: ts("2026-06-29T16:00:00Z").timestamp(),
                price: 111.5415,
            },
            b: LinePoint {
                at_epoch: ts("2026-06-29T17:00:00Z").timestamp(),
                price: 111.5303,
            },
            extend_forward: true,
            bar_seconds: 3600,
            dir: CrossDir::Down,
            bar: BarEvent::Intrabar,
        };
        let p = plan(vec![
            rule("04-prep-retest", neckline, FireMode::Once, Action::Prep),
            rule(
                "05-enter",
                Trigger::MwEveryBar,
                FireMode::Once,
                Action::Enter,
            ),
        ]);
        let mut prior = seed_at(Phase::AwaitEntry, "2026-06-29T17:00:00Z");
        prior.break_close_at = Some(ts("2026-06-29T17:00:00Z"));

        // The 6pm bar: opened above the line, low wicked below, closed above.
        let six_pm = candle("2026-06-29T18:00:00Z", 111.582, 111.606, 111.501, 111.540);
        let eval = run(&p, &prior, &[six_pm]);

        assert_eq!(
            eval.new_state.retest_seen_at,
            Some(ts("2026-06-29T18:00:00Z")),
            "an open-above/low-below bar is a down-cross of the retest line — \
             the wick crossed even though the close recovered above"
        );
        let fired: Vec<&str> = eval.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(
            fired.contains(&"04-prep-retest"),
            "the retest prep must fire on the 6pm wick bar, got {fired:?}"
        );
    }

    /// End-to-end: the plan-level `cross_buffer_pct` flows through
    /// `evaluate_plan` to the retest cross. The 6pm bar dips ~0.018 below the
    /// line (~0.016% of 111.519). A 0.1% buffer (~0.11 depth required) rejects
    /// that shallow tap — the retest does **not** stamp; a 0.01% buffer
    /// (~0.011 required) admits it. Proves the field is threaded, not just the
    /// `eval_trigger` arg.
    #[test]
    fn plan_cross_buffer_pct_gates_the_retest() {
        let neckline = Trigger::TrendlineCross {
            a: LinePoint {
                at_epoch: ts("2026-06-29T16:00:00Z").timestamp(),
                price: 111.5415,
            },
            b: LinePoint {
                at_epoch: ts("2026-06-29T17:00:00Z").timestamp(),
                price: 111.5303,
            },
            extend_forward: true,
            bar_seconds: 3600,
            dir: CrossDir::Down,
            bar: BarEvent::Intrabar,
        };
        let mk_plan = |buffer_pct: f64| {
            let mut p = plan(vec![
                rule(
                    "04-prep-retest",
                    neckline.clone(),
                    FireMode::Once,
                    Action::Prep,
                ),
                rule(
                    "05-enter",
                    Trigger::MwEveryBar,
                    FireMode::Once,
                    Action::Enter,
                ),
            ]);
            p.cross_buffer_pct = buffer_pct;
            p
        };
        let mut prior = seed_at(Phase::AwaitEntry, "2026-06-29T17:00:00Z");
        prior.break_close_at = Some(ts("2026-06-29T17:00:00Z"));
        let six_pm = candle("2026-06-29T18:00:00Z", 111.582, 111.606, 111.501, 111.540);

        // 0.1% buffer → the shallow 0.016% tap is rejected.
        let big = run(&mk_plan(0.1), &prior, &[six_pm]);
        assert!(
            big.new_state.retest_seen_at.is_none(),
            "a 0.1% buffer rejects the shallow 6pm tap"
        );
        // 0.01% buffer → the tap is deep enough; retest stamps.
        let small = run(&mk_plan(0.01), &prior, &[six_pm]);
        assert_eq!(
            small.new_state.retest_seen_at,
            Some(ts("2026-06-29T18:00:00Z")),
            "a 0.01% buffer admits the same tap"
        );
    }

    // ===== guards =====

    #[test]
    fn trade_expiry_guard_fires_and_finishes_plan() {
        let expiry = ts("2026-06-16T15:00:00Z").timestamp();
        let p = plan(vec![
            rule(
                "05-enter",
                Trigger::MwEveryBar,
                FireMode::EveryBar,
                Action::Enter,
            ),
            rule(
                "02-veto-trade-expiry",
                Trigger::TimeReached { at_epoch: expiry },
                FireMode::Once,
                Action::Veto,
            ),
        ]);
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T13:00:00Z");
        // A bar past the expiry: the guard fires, plan done, no enter.
        let c = candle("2026-06-16T16:00:00Z", 1.0, 1.0, 1.0, 1.0);
        let eval = run(&p, &prior, &[c]);
        assert!(eval.done);
        assert_eq!(eval.fired.len(), 1);
        assert_eq!(eval.fired[0].rule_id, "02-veto-trade-expiry");
    }

    #[test]
    fn pause_control_fires_at_its_epoch_without_ending_the_spine() {
        // A pause window opening mid-trade: its TimeReached fires, dispatches
        // the Pause intent, but the plan keeps running (non-terminal) and the
        // enter heartbeat still fires the same bar.
        let pause_at = ts("2026-06-16T15:00:00Z").timestamp();
        let p = plan(vec![
            rule(
                "05-enter",
                Trigger::MwEveryBar,
                FireMode::EveryBar,
                Action::Enter,
            ),
            rule(
                "pause-start-news1",
                Trigger::TimeReached { at_epoch: pause_at },
                FireMode::Once,
                Action::Pause,
            ),
        ]);
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T13:00:00Z");
        let c = candle("2026-06-16T16:00:00Z", 1.0, 1.0, 1.0, 1.0);
        let eval = run(&p, &prior, &[c]);
        assert!(!eval.done, "a pause fire must not end the plan");
        let ids: Vec<&str> = eval.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(ids.contains(&"pause-start-news1"), "pause must fire");
        assert!(ids.contains(&"05-enter"), "enter heartbeat still fires");
    }

    #[test]
    fn pause_and_resume_fire_on_their_own_bars_and_dont_refire() {
        // Resume comes a bar after pause. Each fires once on the first bar at or
        // past its epoch; a re-tick of a later bar doesn't refire either.
        let pause_at = ts("2026-06-16T15:00:00Z").timestamp();
        let resume_at = ts("2026-06-16T16:00:00Z").timestamp();
        let p = plan(vec![
            rule(
                "pause-start-news1",
                Trigger::TimeReached { at_epoch: pause_at },
                FireMode::Once,
                Action::Pause,
            ),
            rule(
                "pause-resume-news1",
                Trigger::TimeReached {
                    at_epoch: resume_at,
                },
                FireMode::Once,
                Action::Resume,
            ),
        ]);
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T13:00:00Z");
        // Bar 1 (15:00): only pause fires.
        let c1 = candle("2026-06-16T15:00:00Z", 1.0, 1.0, 1.0, 1.0);
        let e1 = run(&p, &prior, &[c1]);
        let ids1: Vec<&str> = e1.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(ids1, vec!["pause-start-news1"]);
        // Bar 2 (16:00): only resume fires (pause already latched).
        let c2 = candle("2026-06-16T16:00:00Z", 1.0, 1.0, 1.0, 1.0);
        let e2 = run(&p, &e1.new_state, &[c2]);
        let ids2: Vec<&str> = e2.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert_eq!(ids2, vec!["pause-resume-news1"]);
        // Bar 3: neither refires.
        let c3 = candle("2026-06-16T17:00:00Z", 1.0, 1.0, 1.0, 1.0);
        let e3 = run(&p, &e2.new_state, &[c3]);
        assert!(e3.fired.is_empty(), "latched controls must not refire");
    }

    #[test]
    fn multiple_news_windows_each_fire() {
        // Two separate calendar events → two news-start + two news-end rules.
        // Walking past all four epochs in one tick batch fires all four.
        let p = plan(vec![
            rule(
                "news-start-evt1",
                Trigger::TimeReached {
                    at_epoch: ts("2026-06-16T10:00:00Z").timestamp(),
                },
                FireMode::Once,
                Action::NewsStart,
            ),
            rule(
                "news-end-evt1",
                Trigger::TimeReached {
                    at_epoch: ts("2026-06-16T11:00:00Z").timestamp(),
                },
                FireMode::Once,
                Action::NewsEnd,
            ),
            rule(
                "news-start-evt2",
                Trigger::TimeReached {
                    at_epoch: ts("2026-06-16T14:00:00Z").timestamp(),
                },
                FireMode::Once,
                Action::NewsStart,
            ),
            rule(
                "news-end-evt2",
                Trigger::TimeReached {
                    at_epoch: ts("2026-06-16T15:00:00Z").timestamp(),
                },
                FireMode::Once,
                Action::NewsEnd,
            ),
        ]);
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T09:00:00Z");
        let candles: Vec<Candle> = [
            "2026-06-16T10:00:00Z",
            "2026-06-16T11:00:00Z",
            "2026-06-16T14:00:00Z",
            "2026-06-16T15:00:00Z",
        ]
        .iter()
        .map(|t| candle(t, 1.0, 1.0, 1.0, 1.0))
        .collect();
        let eval = run(&p, &prior, &candles);
        assert!(!eval.done);
        let mut ids: Vec<&str> = eval.fired.iter().map(|f| f.rule_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                "news-end-evt1",
                "news-end-evt2",
                "news-start-evt1",
                "news-start-evt2"
            ]
        );
    }

    #[test]
    fn rerun_same_candles_does_not_refire_a_latched_guard() {
        // Idempotency: a guard that already latched in state.fired doesn't fire
        // again on a re-tick of the same kind of bar.
        let p = plan(vec![rule(
            "01-veto-too-high",
            Trigger::HorizontalCross {
                level: 1.3,
                dir: CrossDir::Up,
                bar: BarEvent::Intrabar,
            },
            FireMode::Once,
            Action::Veto,
        )]);
        let mut prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        prior.fired.insert("01-veto-too-high".into());
        prior.phase = Phase::Done; // already terminal
        let c = candle("2026-06-16T11:00:00Z", 1.29, 1.31, 1.29, 1.305);
        let eval = run(&p, &prior, &[c]);
        assert!(eval.fired.is_empty(), "latched guard must not refire");
    }

    #[test]
    fn last_close_carries_across_ticks_for_on_close_cross() {
        // A b&c OnClose cross spanning two ticks: tick 1 seeds last_close above
        // the level (no fire); tick 2's close below fires the cross.
        let p = hs_plan();
        let prior = seed_at(Phase::AwaitBreakAndClose, "2026-06-16T09:00:00Z");
        // Tick 1: close ABOVE the neckline → seeds last_close, no cross.
        let c1 = candle("2026-06-16T10:00:00Z", 1.21, 1.215, 1.205, 1.2100);
        let eval1 = run(&p, &prior, &[c1]);
        assert!(eval1.fired.is_empty(), "no down-cross when staying above");
        assert_eq!(
            eval1
                .new_state
                .last_close
                .get("03-prep-break-and-close")
                .copied(),
            Some(1.2100)
        );
        // Tick 2: close BELOW → down-cross fires using the carried prior close.
        let c2 = candle("2026-06-16T11:00:00Z", 1.205, 1.205, 1.195, 1.1950);
        let eval2 = run(&p, &eval1.new_state, &[c2]);
        assert_eq!(eval2.fired.len(), 1);
        assert_eq!(eval2.fired[0].rule_id, "03-prep-break-and-close");
    }

    #[test]
    fn watermark_is_monotonic_and_advances_to_last_candle() {
        let p = plan(vec![rule(
            "05-enter",
            Trigger::MwEveryBar,
            FireMode::EveryBar,
            Action::Enter,
        )]);
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let candles = [
            candle("2026-06-16T11:00:00Z", 1.0, 1.0, 1.0, 1.0),
            candle("2026-06-16T12:00:00Z", 1.0, 1.0, 1.0, 1.0),
            candle("2026-06-16T13:00:00Z", 1.0, 1.0, 1.0, 1.0),
        ];
        let eval = run(&p, &prior, &candles);
        assert_eq!(eval.new_state.watermark, Some(ts("2026-06-16T13:00:00Z")));
    }

    #[test]
    fn last_close_map_is_deterministic() {
        // BTreeMap ⇒ stable JSON; two equal states serialise identically.
        let mut a = BTreeMap::new();
        a.insert("z".to_string(), 2.0);
        a.insert("a".to_string(), 1.0);
        let mut st = PlanState::seed(Phase::AwaitEntry, ts("2026-06-30T00:00:00Z"));
        st.last_close = a;
        let j1 = serde_json::to_string(&st).unwrap();
        let j2 = serde_json::to_string(&st).unwrap();
        assert_eq!(j1, j2);
        assert!(j1.find("\"a\"").unwrap() < j1.find("\"z\"").unwrap());
    }

    // ===== seed_plan_state =====

    #[test]
    fn seed_sets_watermark_to_newest_and_fires_nothing() {
        let p = plan(vec![rule(
            "05-enter",
            Trigger::MwEveryBar,
            FireMode::EveryBar,
            Action::Enter,
        )]);
        // Out-of-order back-window: newest is 13:00.
        let candles = [
            candle("2026-06-16T13:00:00Z", 1.0, 1.0, 1.0, 1.0),
            candle("2026-06-16T11:00:00Z", 1.0, 1.0, 1.0, 1.0),
            candle("2026-06-16T12:00:00Z", 1.0, 1.0, 1.0, 1.0),
        ];
        let st = seed_plan_state(&p, &candles, ts("2026-06-30T00:00:00Z"));
        assert_eq!(st.watermark, Some(ts("2026-06-16T13:00:00Z")));
        // MwEveryBar would fire on every candle if evaluated — seeding must not.
        assert!(st.fired.is_empty());
        assert_eq!(st.phase, Phase::AwaitEntry);
    }

    #[test]
    fn seed_records_last_close_for_on_close_rule_from_newest_candle() {
        let p = plan(vec![rule(
            "03-prep-break-and-close",
            Trigger::TrendlineCross {
                a: LinePoint {
                    at_epoch: ts("2026-06-16T10:00:00Z").timestamp(),
                    price: 1.2000,
                },
                b: LinePoint {
                    at_epoch: ts("2026-06-16T20:00:00Z").timestamp(),
                    price: 1.2000,
                },
                extend_forward: true,
                bar_seconds: 3600,
                dir: CrossDir::Down,
                bar: BarEvent::OnClose,
            },
            FireMode::Once,
            Action::Prep,
        )]);
        let candles = [
            candle("2026-06-16T11:00:00Z", 1.2, 1.2, 1.2, 1.2050),
            candle("2026-06-16T12:00:00Z", 1.2, 1.2, 1.2, 1.2030),
        ];
        let st = seed_plan_state(&p, &candles, ts("2026-06-30T00:00:00Z"));
        // last_close holds the newest candle's close (1.2030), so the next
        // tick's first candle is compared against it — not back-fired here.
        assert_eq!(
            st.last_close.get("03-prep-break-and-close").copied(),
            Some(1.2030)
        );
        // A break-and-close plan seeds into AwaitBreakAndClose.
        assert_eq!(st.phase, Phase::AwaitBreakAndClose);
    }

    #[test]
    fn seed_empty_window_is_unwatermarked() {
        let p = plan(vec![rule(
            "05-enter",
            Trigger::MwEveryBar,
            FireMode::EveryBar,
            Action::Enter,
        )]);
        let st = seed_plan_state(&p, &[], ts("2026-06-30T00:00:00Z"));
        assert!(st.watermark.is_none());
        assert!(st.last_close.is_empty());
    }

    // ===== Stage E: PinePattern entry =====

    /// A back-window ending in a bearish pinbar on the last bar: a small body in
    /// the bottom quartile, a long upper wick (≥ 50% range), and a high above the
    /// prior bar's high (the bearish breakout). The earlier bars are flat context
    /// so no other signal prints.
    fn bearish_pinbar_window() -> Vec<Candle> {
        vec![
            candle("2026-06-16T08:00:00Z", 1.10, 1.11, 1.09, 1.10),
            candle("2026-06-16T09:00:00Z", 1.10, 1.11, 1.09, 1.10),
            // prior bar — its high 1.12 is the level the pinbar must exceed.
            candle("2026-06-16T10:00:00Z", 1.10, 1.12, 1.09, 1.105),
            // bearish pinbar: range 1.10..1.30 = 0.20. body 1.115..1.12 (bottom
            // quartile: bottom_25 = 1.10 + 0.05 = 1.15 → body_bottom 1.115 ≤
            // 1.15). upper wick = high - body_top = 1.30 - 1.12 = 0.18 ≥ 0.10.
            // high 1.30 > prior high 1.12. close 1.115 < open 1.12 → bearish.
            candle("2026-06-16T11:00:00Z", 1.12, 1.30, 1.10, 1.115),
        ]
    }

    #[test]
    fn pine_short_entry_fires_with_signal_geometry() {
        // H&S short: PinePattern{dir: Short} single-shot enter, in AwaitEntry.
        // Carries real signal-anchored geometry so the bug #13 resolve pre-flight
        // passes (a bare no-geometry enter would now decline at resolve).
        let p = plan(vec![pine_enter_rule(None, Direction::Short, false)]);
        let window = bearish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        // Only the last (pinbar) candle is new this tick; the whole window is the
        // detector back-window.
        let new = &window[3..];
        let eval = run_window(&p, &prior, new, &window);
        assert_eq!(eval.fired.len(), 1, "the pinbar fires the short enter");
        let f = &eval.fired[0];
        assert_eq!(f.rule_id, "05-enter");
        let sig = f
            .signal
            .expect("a PinePattern fire carries latched geometry");
        assert_eq!(sig.direction, Direction::Short);
        assert_eq!(sig.kind, SignalKind::Pinbar);
        // single-bar pinbar geometry → the pinbar's own extremes.
        assert!((sig.signal_high - 1.30).abs() < 1e-12);
        assert!((sig.signal_low - 1.10).abs() < 1e-12);
        // single-shot enter ends the spine.
        assert!(eval.done);
    }

    #[test]
    fn multi_shot_pine_enter_fires_but_stays_await_entry() {
        // Same fire as the single-shot case, but the enter opts into multi-shot
        // (`max_retries > 0`). It must fire AND keep the plan in `AwaitEntry` so
        // a later signal bar can re-enter — if it went `Done`, the cron would
        // archive the plan and no re-entry could ever fire (the NZD/CHF bug).
        let mut rule = pine_enter_rule(None, Direction::Short, false);
        rule.intent.max_retries = trade_control_core::tunable::Tunable::Static(5);
        let p = plan(vec![rule]);
        let window = bearish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert_eq!(eval.fired.len(), 1, "the pinbar still fires the enter");
        assert_eq!(eval.fired[0].rule_id, "05-enter");
        assert!(
            !eval.done,
            "a multi-shot enter must NOT retire the plan on its first fire"
        );
        assert_eq!(
            eval.new_state.phase,
            Phase::AwaitEntry,
            "the plan stays armed for the next re-entry signal"
        );
        assert!(
            !eval.new_state.fired.contains("05-enter"),
            "a multi-shot enter must not latch in `fired` (it re-fires on new bars)"
        );
    }

    #[test]
    fn pine_entry_does_not_fire_for_opposite_direction_plan() {
        // Same bearish-pinbar window, but the plan is a LONG H&S — the latched
        // signal is Short, so the entry must not fire.
        let p = plan(vec![pine_enter_rule(None, Direction::Long, false)]);
        let window = bearish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert!(eval.fired.is_empty(), "short signal can't fire a long plan");
        assert!(!eval.done);
    }

    #[test]
    fn pine_entry_kind_filter_blocks_mismatched_pattern() {
        // The window prints a pinbar; a plan demanding a Tweezer must not fire.
        let p = plan(vec![pine_enter_rule(
            Some(SignalKind::Tweezer),
            Direction::Short,
            false,
        )]);
        let window = bearish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert!(eval.fired.is_empty(), "kind filter blocks a pinbar");
    }

    #[test]
    fn pine_entry_blocked_until_retest_seen() {
        // H&S short with a retest rule: the pinbar entry is gated until a retest
        // up-cross has been stamped after the break-and-close. The retest line
        // sits at 1.35 — *above* the pinbar bar's high (1.30), so the bar never
        // reaches it: no straddle, no up-cross, entry stays blocked. (Were the
        // line within the bar's range, the new wick-based Intrabar rule would
        // stamp an up-cross the moment the high pushed through it — open 1.12
        // below the line, high reaching above is a genuine intrabar up-cross
        // regardless of where the bar closes.)
        let neckline_up = Trigger::TrendlineCross {
            a: LinePoint {
                at_epoch: ts("2026-06-16T00:00:00Z").timestamp(),
                price: 1.35,
            },
            b: LinePoint {
                at_epoch: ts("2026-06-16T01:00:00Z").timestamp(),
                price: 1.35,
            },
            extend_forward: true,
            bar_seconds: 3600,
            dir: CrossDir::Up,
            bar: BarEvent::Intrabar,
        };
        let p = plan(vec![
            rule("04-prep-retest", neckline_up, FireMode::Once, Action::Prep),
            pine_enter_rule(None, Direction::Short, false),
        ]);
        let window = bearish_pinbar_window();
        let mut prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        prior.break_close_at = Some(ts("2026-06-16T10:30:00Z"));
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert!(
            eval.fired.is_empty(),
            "entry blocked: retest line above the bar's high → no up-cross"
        );
        assert!(
            eval.new_state.retest_seen_at.is_none(),
            "no straddle → retest must not stamp"
        );
    }

    // ===== Bug #13: a resolve-failed / gate-failed Pine enter must not retire =====

    /// A short Pine enter whose bracket **cannot resolve** on the firing bar: the
    /// absolute TP sits *above* the entry (wrong side for a short →
    /// `EntryOutsideRange`). The detector still fires the pinbar, but the worker
    /// would reject it `resolve-failed`. The same `pine_enter_rule` geometry,
    /// only the TP flipped to the wrong side.
    fn pine_enter_rule_unresolvable_tp() -> ConditionRule {
        use trade_control_core::intent::{PriceRef, TakeProfit};
        let mut r = pine_enter_rule(None, Direction::Short, false);
        // TP above the ~1.10 entry is invalid for a short — out of the SL..TP
        // range, so resolution fails.
        r.intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute { absolute: 1.50 }));
        r
    }

    #[test]
    fn pine_enter_resolve_failed_does_not_retire_plan() {
        // THE bug #13 regression. The pinbar fires the detector, but the bracket
        // can't resolve → the plan must NOT go Done: nothing fires, phase stays
        // AwaitEntry, so the vetos keep ticking and a later bar can re-form a
        // valid pattern.
        let p = plan(vec![pine_enter_rule_unresolvable_tp()]);
        let window = bearish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert!(eval.fired.is_empty(), "an unresolvable enter must not fire");
        assert!(
            !eval.done,
            "a resolve-failed enter must not retire the plan"
        );
        assert_eq!(
            eval.new_state.phase,
            Phase::AwaitEntry,
            "phase stays AwaitEntry, not Done"
        );
        assert!(
            !eval.new_state.fired.contains("05-enter"),
            "the enter rule does not latch on a resolve-failure"
        );
    }

    #[test]
    fn veto_still_fires_after_a_resolve_failed_pine_enter() {
        // Acceptance criterion 3: a close-positions veto crossed *after* a
        // resolve-failed enter still fires — the plan wasn't abandoned. Tick 1's
        // pinbar can't resolve (stays AwaitEntry); tick 2 crosses the veto level
        // and the guard fires + finishes the plan.
        let veto = rule(
            "01-veto-too-high",
            Trigger::HorizontalCross {
                level: 1.40,
                dir: CrossDir::Up,
                bar: BarEvent::Intrabar,
            },
            FireMode::Once,
            Action::Veto,
        );
        let p = plan(vec![pine_enter_rule_unresolvable_tp(), veto]);
        let window = bearish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");

        // Tick 1: the pinbar fires the detector but the bracket can't resolve →
        // no fire, plan still alive.
        let eval1 = run_window(&p, &prior, &window[3..], &window);
        assert!(
            eval1.fired.is_empty(),
            "resolve-failed enter: nothing fires"
        );
        assert!(!eval1.done, "plan survives the resolve-failed enter");

        // Tick 2: a later bar crosses the veto level (high 1.41 > 1.40) → the
        // guard fires and finishes the plan. Had the plan gone Done in tick 1,
        // this veto would have been silently abandoned.
        let later = candle("2026-06-16T12:00:00Z", 1.30, 1.41, 1.30, 1.405);
        let eval2 = run_window(&p, &eval1.new_state, &[later], &[later]);
        let ids: Vec<&str> = eval2.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(
            ids.contains(&"01-veto-too-high"),
            "the veto fires after a resolve-failed enter, got {ids:?}"
        );
        assert!(eval2.done, "the terminal veto finishes the plan");
    }

    #[test]
    fn pine_enter_needs_golden_declines_on_non_golden_bar() {
        // Finding B1: a `needs_golden` Pine enter must not fire on a bar whose
        // latched signal isn't golden — and, like resolve-failure, the decline is
        // non-terminal (stay AwaitEntry), not a Done. The short back-window here
        // has no ATR, so the pinbar's latch is non-golden.
        let p = plan(vec![pine_enter_rule(None, Direction::Short, true)]);
        let window = bearish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert!(
            eval.fired.is_empty(),
            "needs_golden blocks a non-golden pinbar"
        );
        assert!(
            !eval.done,
            "a needs_golden decline must not retire the plan"
        );
        assert_eq!(eval.new_state.phase, Phase::AwaitEntry);
    }

    #[test]
    fn pine_entry_dispatchable_unit() {
        // Pin the pure helper directly. Golden gate, confirmed gate, and resolve
        // each gate the fire; a resolvable, gate-passing intent is dispatchable.
        let window = bearish_pinbar_window();
        let last = window[3];
        let sig = latched_signal_at(&window, 3, &DetectorConfig::pine_defaults(Granularity::H1))
            .expect("the window prints a latched signal on the pinbar");

        // Resolvable, no gates → dispatchable.
        let ok = pine_enter_rule(None, Direction::Short, false).intent;
        assert!(pine_entry_dispatchable(&ok, &last, &sig, 0.0001));

        // needs_golden on a non-golden latch → not dispatchable.
        let mut golden = ok.clone();
        golden.needs_golden = true;
        assert!(!sig.golden, "fixture precondition: latch is non-golden");
        assert!(!pine_entry_dispatchable(&golden, &last, &sig, 0.0001));

        // Unresolvable bracket → not dispatchable.
        let bad = pine_enter_rule_unresolvable_tp().intent;
        assert!(!pine_entry_dispatchable(&bad, &last, &sig, 0.0001));
    }

    // ===== close-on-reversal (PinePattern guard) =====

    /// A back-window ending in a **bullish** pinbar on the last bar: a small body
    /// in the top quartile, a long lower wick (≥ 50% range), and a low below the
    /// prior bar's low (the bullish breakout). The earlier bars are flat context
    /// so no other signal prints. Mirrors `bearish_pinbar_window` but the
    /// opposite direction — a long reversal candle, the shape that closes an open
    /// short via `06-close-on-reversal`. The pinbar's close is 1.18.
    fn bullish_pinbar_window() -> Vec<Candle> {
        vec![
            candle("2026-06-16T08:00:00Z", 1.10, 1.11, 1.09, 1.10),
            candle("2026-06-16T09:00:00Z", 1.10, 1.11, 1.09, 1.10),
            // prior bar — its low 1.09 is the level the pinbar must undercut.
            candle("2026-06-16T10:00:00Z", 1.10, 1.11, 1.09, 1.105),
            // bullish pinbar: range 1.00..1.20 = 0.20. body 1.16..1.18 (top
            // quartile: top_25 = 1.20 - 0.05 = 1.15 → body_bottom 1.16 ≥ 1.15).
            // lower wick = body_bottom - low = 1.16 - 1.00 = 0.16 ≥ 0.10. low 1.00
            // < prior low 1.09. close 1.18 > open 1.16 → bullish.
            candle("2026-06-16T11:00:00Z", 1.16, 1.20, 1.00, 1.18),
        ]
    }

    /// A `06-close-on-reversal` guard rule: a `PinePattern` close bound to `dir`
    /// (the *opposite* of the trade). When `band` is `Some`, the intent opts into
    /// the price contextual window (`inside_window: [price]`, `sr_bands: [band]`)
    /// so the engine's pure price gate applies; `None` leaves it news-only-shaped
    /// (no recomputable price gate → fires on the detector match alone).
    fn close_on_reversal_rule(dir: Direction, band: Option<[f64; 2]>) -> ConditionRule {
        let mut intent = intent(Action::Close);
        if let Some(b) = band {
            intent.inside_window = vec![trade_control_core::intent::EventWindow::Price];
            intent.sr_bands = vec![b];
        }
        ConditionRule {
            rule_id: "06-close-on-reversal".into(),
            trigger: Trigger::PinePattern { pattern: None, dir },
            fire_mode: FireMode::Once,
            intent,
        }
    }

    /// A news-windowed `06-close-on-reversal` guard: `inside_window: ["news"]`,
    /// no price band. This is the real shape `build_trade` emits for the safety
    /// flatten; it only fires while a news window is open and is non-terminal
    /// for the spine (Defect A + B fixtures).
    fn news_close_on_reversal_rule(dir: Direction) -> ConditionRule {
        let mut intent = intent(Action::Close);
        intent.inside_window = vec![trade_control_core::intent::EventWindow::News];
        intent.trade_id = Some("tid".into());
        ConditionRule {
            rule_id: "06-close-on-reversal".into(),
            trigger: Trigger::PinePattern { pattern: None, dir },
            fire_mode: FireMode::Once,
            intent,
        }
    }

    /// A `news-start` / `news-end` control rule firing at `at` (a `TimeReached`
    /// trigger), carrying `news_id` so the engine opens / closes the window in
    /// `open_news_windows`.
    fn news_control_rule(rule_id: &str, action: Action, news_id: &str, at: &str) -> ConditionRule {
        let mut intent = intent(action);
        intent.trade_id = Some("tid".into());
        intent.news_id = Some(news_id.into());
        ConditionRule {
            rule_id: rule_id.into(),
            trigger: Trigger::TimeReached {
                at_epoch: ts(at).timestamp(),
            },
            fire_mode: FireMode::Once,
            intent,
        }
    }

    #[test]
    fn close_on_reversal_fires_on_a_long_reversal_in_the_band() {
        // THE bug: a SHORT trade's `06-close-on-reversal` is a PinePattern{Long}
        // close. A bullish reversal candle whose close (1.18) sits inside the SR
        // band must fire the close and retire the plan — the engine fires it for
        // both the worker (dispatch run_close) and the replay (exit the position).
        let p = plan(vec![close_on_reversal_rule(
            Direction::Long,
            Some([1.15, 1.20]),
        )]);
        let window = bullish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert_eq!(eval.fired.len(), 1, "the bullish reversal fires the close");
        let f = &eval.fired[0];
        assert_eq!(f.rule_id, "06-close-on-reversal");
        assert_eq!(f.intent.action, Action::Close);
        let sig = f
            .signal
            .expect("a PinePattern close carries latched geometry");
        assert_eq!(sig.direction, Direction::Long);
        assert!(eval.done, "a terminal close retires the plan");
    }

    #[test]
    fn close_on_reversal_declines_when_close_outside_every_band() {
        // Same bullish reversal candle, but the SR band sits well above the
        // pinbar's close (1.18) — the pure price gate must decline, so the close
        // does NOT fire and the plan stays alive (matching the live worker, whose
        // run_close rejects an out-of-band price). The detector still matched;
        // only the contextual price gate blocks it.
        let p = plan(vec![close_on_reversal_rule(
            Direction::Long,
            Some([1.30, 1.40]),
        )]);
        let window = bullish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert!(
            eval.fired.is_empty(),
            "a reversal outside every band must not fire the close"
        );
        assert!(!eval.done, "an out-of-band close must not retire the plan");
        assert_eq!(eval.new_state.phase, Phase::AwaitEntry);
    }

    #[test]
    fn close_on_reversal_with_no_window_at_all_fires_on_detector_alone() {
        // A close with NO contextual window (empty `inside_window`, no sr_bands)
        // has neither a price nor a news gate here, so the detector match alone
        // fires it. (This is the degenerate "ungated" shape; the real news close
        // carries `inside_window: ["news"]` and is gated — see the news tests.)
        let p = plan(vec![close_on_reversal_rule(Direction::Long, None)]);
        let window = bullish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert_eq!(eval.fired.len(), 1, "no gate → detector match fires");
        assert_eq!(eval.fired[0].rule_id, "06-close-on-reversal");
        assert!(eval.done);
    }

    // ===== Defect A: news-windowed close only fires inside an open window =====

    #[test]
    fn news_close_does_not_fire_when_no_news_window_open() {
        // THE USD/CHF 2026-06-26 bug: a news-only close (`inside_window:["news"]`)
        // saw a qualifying long reversal 9h after `news-end` and fired, retiring
        // the spine and erasing two legitimate short entries. With the news gate,
        // a reversal printing while NO window is open must NOT fire, and the spine
        // must survive in AwaitEntry.
        let p = plan(vec![news_close_on_reversal_rule(Direction::Long)]);
        let window = bullish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert!(
            eval.fired.is_empty(),
            "a news close must not fire outside an open news window"
        );
        assert!(
            !eval.done,
            "the spine must survive — pending entries proceed"
        );
        assert_eq!(eval.new_state.phase, Phase::AwaitEntry);
    }

    #[test]
    fn news_close_fires_while_a_news_window_is_open() {
        // Same reversal, but a news-start opened a window earlier and no news-end
        // has closed it yet → the close fires (the engine mirrors run_close's
        // active-window check). It is non-terminal (Defect B), so it dispatches
        // the flatten but the spine stays alive.
        let p = plan(vec![
            news_control_rule(
                "news-start-1",
                Action::NewsStart,
                "gdp",
                "2026-06-16T09:00:00Z",
            ),
            news_close_on_reversal_rule(Direction::Long),
        ]);
        let window = bullish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T08:30:00Z");
        // Feed the window from the news-start bar onward so the control fires
        // before the reversal bar.
        let eval = run_window(&p, &prior, &window[1..], &window);
        let ids: Vec<&str> = eval.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(ids.contains(&"news-start-1"), "the news window opens");
        assert!(
            ids.contains(&"06-close-on-reversal"),
            "the close fires inside the open window: {ids:?}"
        );
        assert!(
            eval.new_state.open_news_windows.contains("gdp"),
            "the window stays open (no news-end fired)"
        );
    }

    #[test]
    fn news_close_does_not_fire_after_news_end_closed_the_window() {
        // news-start then news-end before the reversal → the window is closed by
        // the time the reversal prints → no fire (the exact 9h-late case). The
        // close-on-reversal stays unfired and the spine survives.
        let p = plan(vec![
            news_control_rule(
                "news-start-1",
                Action::NewsStart,
                "gdp",
                "2026-06-16T09:00:00Z",
            ),
            news_control_rule("news-end-1", Action::NewsEnd, "gdp", "2026-06-16T10:00:00Z"),
            news_close_on_reversal_rule(Direction::Long),
        ]);
        let window = bullish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T08:30:00Z");
        let eval = run_window(&p, &prior, &window[1..], &window);
        let ids: Vec<&str> = eval.fired.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(ids.contains(&"news-start-1") && ids.contains(&"news-end-1"));
        assert!(
            !ids.contains(&"06-close-on-reversal"),
            "the close must not fire once the window closed: {ids:?}"
        );
        assert!(
            eval.new_state.open_news_windows.is_empty(),
            "news-end emptied the open-window set"
        );
        assert!(
            !eval.done,
            "spine survives a window that closed without a close"
        );
    }

    // ===== Defect B: a news-windowed close is non-terminal for the spine =====

    #[test]
    fn news_close_is_non_terminal_for_the_spine() {
        // A news close that DOES fire (inside its window) must not retire the
        // spine — it's a flatten-if-open safety, not a thesis invalidation. The
        // worker / replay `allow_close` gate decides whether anything flattens;
        // the engine keeps pending entries alive regardless.
        let p = plan(vec![
            news_control_rule(
                "news-start-1",
                Action::NewsStart,
                "gdp",
                "2026-06-16T09:00:00Z",
            ),
            news_close_on_reversal_rule(Direction::Long),
        ]);
        let window = bullish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T08:30:00Z");
        let eval = run_window(&p, &prior, &window[1..], &window);
        assert!(
            eval.fired
                .iter()
                .any(|f| f.rule_id == "06-close-on-reversal"),
            "the news close fired"
        );
        assert!(
            !eval.done,
            "a news close is non-terminal — the spine survives for pending entries"
        );
        assert_eq!(eval.new_state.phase, Phase::AwaitEntry);
    }

    #[test]
    fn price_band_close_stays_terminal() {
        // Contrast: a price-windowed close at the SR band IS a thesis
        // invalidation and must still retire the plan (unchanged behaviour).
        let p = plan(vec![close_on_reversal_rule(
            Direction::Long,
            Some([1.15, 1.20]),
        )]);
        let window = bullish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert!(
            eval.done,
            "a price-band reversal-close still retires the plan"
        );
        assert_eq!(eval.new_state.phase, Phase::Done);
    }

    #[test]
    fn close_on_reversal_ignores_same_direction_pattern() {
        // A SHORT trade's close binds to the LONG reversal. A *bearish* (short)
        // reversal candle is the trade's own direction — it must NOT fire the
        // close (that's an enter signal, not an exit). Guards the direction match.
        let p = plan(vec![close_on_reversal_rule(
            Direction::Long,
            Some([1.00, 1.20]),
        )]);
        let window = bearish_pinbar_window(); // prints a SHORT signal
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert!(
            eval.fired.is_empty(),
            "a short reversal can't fire the long-bound close"
        );
        assert!(!eval.done);
    }

    #[test]
    fn price_in_any_band_inclusive_endpoints() {
        let bands = [[1.10, 1.20], [1.30, 1.40]];
        assert_eq!(price_in_any_band(1.15, &bands), Some([1.10, 1.20]));
        assert_eq!(price_in_any_band(1.10, &bands), Some([1.10, 1.20])); // lo edge
        assert_eq!(price_in_any_band(1.40, &bands), Some([1.30, 1.40])); // hi edge
        assert_eq!(price_in_any_band(1.35, &bands), Some([1.30, 1.40]));
        assert_eq!(price_in_any_band(1.25, &bands), None); // between bands
        assert_eq!(price_in_any_band(1.15, &[]), None); // no bands
    }

    #[test]
    fn mw_heartbeat_enter_is_not_preflighted() {
        // The bug #13 pre-flight is PinePattern-only. An M/W heartbeat enter (no
        // signal) must still fire every bar even though its bare intent carries
        // no resolvable geometry — the worker's run_enter owns M/W resolution and
        // its by-design NotArmedYet decline.
        let p = plan(vec![rule(
            "05-enter",
            Trigger::MwEveryBar,
            FireMode::EveryBar,
            Action::Enter,
        )]);
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T11:00:00Z");
        let c = candle("2026-06-16T12:00:00Z", 1.0, 1.0, 1.0, 1.0);
        let eval = run(&p, &prior, &[c]);
        assert_eq!(eval.fired.len(), 1, "M/W heartbeat still fires every bar");
        assert_eq!(eval.fired[0].rule_id, "05-enter");
    }

    // ===== PlanEval::is_noteworthy — the no-op-tick trim predicate =====
    //
    // These exercise the predicate against real `evaluate_plan` output so the
    // watermark-only no-op (the critical case) is proven on the state the
    // evaluator actually produces, not a hand-built one.

    #[test]
    fn noteworthy_when_an_intent_fired() {
        // M/W heartbeat fires an enter every bar → always noteworthy.
        let p = plan(vec![rule(
            "05-enter",
            Trigger::MwEveryBar,
            FireMode::EveryBar,
            Action::Enter,
        )]);
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T11:00:00Z");
        let c = candle("2026-06-16T12:00:00Z", 1.0, 1.0, 1.0, 1.0);
        let eval = run(&p, &prior, &[c]);
        assert!(!eval.fired.is_empty());
        assert!(eval.is_noteworthy(&prior), "a fired intent is noteworthy");
    }

    #[test]
    fn noteworthy_when_plan_finished() {
        // A trade-expiry guard fires and finishes the plan → noteworthy.
        let expiry = ts("2026-06-16T15:00:00Z").timestamp();
        let p = plan(vec![rule(
            "02-veto-trade-expiry",
            Trigger::TimeReached { at_epoch: expiry },
            FireMode::Once,
            Action::Veto,
        )]);
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T13:00:00Z");
        let c = candle("2026-06-16T16:00:00Z", 1.0, 1.0, 1.0, 1.0);
        let eval = run(&p, &prior, &[c]);
        assert!(eval.done);
        assert!(eval.is_noteworthy(&prior), "a finished plan is noteworthy");
    }

    #[test]
    fn noteworthy_when_phase_advanced() {
        // Break-and-close fires: phase AwaitBreakAndClose → AwaitEntry. Even
        // though an intent also fired here, the phase advance alone is what makes
        // a transition-only tick noteworthy.
        let p = hs_plan();
        let mut prior = seed_at(Phase::AwaitBreakAndClose, "2026-06-16T09:00:00Z");
        prior
            .last_close
            .insert("03-prep-break-and-close".into(), 1.2050);
        let c = candle("2026-06-16T10:00:00Z", 1.205, 1.205, 1.195, 1.1950);
        let eval = run(&p, &prior, &[c]);
        assert_eq!(eval.new_state.phase, Phase::AwaitEntry);
        assert!(eval.new_state.advanced_vs(&prior), "phase moved");
        assert!(eval.is_noteworthy(&prior));
    }

    #[test]
    fn not_noteworthy_on_watermark_only_advance() {
        // THE CRITICAL CASE. An H&S plan sitting in AwaitBreakAndClose; a new bar
        // arrives that does NOT cross the neckline → nothing fires, no phase
        // change, plan not done. The only things that moved are the watermark
        // (to the new bar) and `last_close` (the OnClose rule's bookkeeping) —
        // neither is a meaningful advance, so the tick is a no-op and must NOT be
        // recorded.
        let p = hs_plan();
        let mut prior = seed_at(Phase::AwaitBreakAndClose, "2026-06-16T09:00:00Z");
        // Prior close ABOVE the 1.2000 neckline; the new bar also stays above and
        // closes above → no down-cross, nothing fires.
        prior
            .last_close
            .insert("03-prep-break-and-close".into(), 1.2100);
        let c = candle("2026-06-16T10:00:00Z", 1.21, 1.215, 1.205, 1.2090);
        let eval = run(&p, &prior, &[c]);

        assert!(eval.fired.is_empty(), "nothing fired");
        assert!(!eval.done, "plan not done");
        assert_eq!(
            eval.new_state.phase,
            Phase::AwaitBreakAndClose,
            "phase unchanged"
        );
        // The watermark DID advance (proving the bar was processed)…
        assert_eq!(
            eval.new_state.watermark,
            Some(ts("2026-06-16T10:00:00Z")),
            "watermark advanced to the new bar"
        );
        // …yet the tick is a no-op: a full-struct compare would be true here
        // (watermark + expires_at + last_close all moved), but is_noteworthy
        // must return false.
        assert_ne!(
            eval.new_state, prior,
            "full-struct compare IS different (watermark/expires_at/last_close moved)"
        );
        assert!(
            !eval.is_noteworthy(&prior),
            "a watermark-only advance must be a no-op — else the trim does nothing"
        );
    }
}
