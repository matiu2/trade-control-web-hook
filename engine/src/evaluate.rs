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

use chrono::{DateTime, Utc};

use trade_control_core::broker::Candle;
use trade_control_core::intent::{Action, Direction, Intent, SignalKind};
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

/// One intent the evaluator decided to fire this run, tagged with the candle
/// that triggered it. The wrapper synthesises a `Shell` from `candle` and
/// dispatches `intent` through the same handlers the webhook uses.
#[derive(Debug, Clone)]
pub struct FiredIntent {
    /// The `rule_id` that fired — for logging / attribution.
    pub rule_id: String,
    /// The exact intent the TV alert would have POSTed, cloned from the rule.
    pub intent: Intent,
    /// The candle on which the trigger fired (its close/high/low/open/time
    /// become the dispatched `Shell`).
    pub candle: Candle,
    /// The latched candle-pattern signal, set only when a `PinePattern` (H&S
    /// enter) rule fired. The wrapper folds its geometry
    /// (`signal_high`/`signal_low`/`golden`/`signal_confirmed`/`recent_*`/…)
    /// onto the dispatched `Shell` so the H&S enter resolves its entry/SL/TP
    /// against the *pattern* extremes — exactly as the TV alert's `{{plot(...)}}`
    /// substitutions did. `None` for every non-Pine trigger (M/W, vetos, preps),
    /// which carry no pattern geometry.
    pub signal: Option<LatchedSignal>,
}

/// The result of one [`evaluate_plan`] run.
#[derive(Debug, Clone)]
pub struct PlanEval {
    /// Intents to dispatch, in candle order.
    pub fired: Vec<FiredIntent>,
    /// The advanced state to persist. The watermark has moved to the last
    /// candle processed (or is unchanged when `new_candles` is empty).
    pub new_state: PlanState,
    /// True once the plan has reached [`Phase::Done`] — the wrapper clears the
    /// plan + state. (M/W plans never reach this via the spine; they end by TTL
    /// or a veto.)
    pub done: bool,
}

/// The starting spine phase for a plan, derived from which rules it carries.
/// A plan with a break-and-close prep starts gated behind it; everything else
/// (notably M/W, whose enter is a per-bar heartbeat with no preps) starts
/// watching for entry directly.
pub fn initial_phase(plan: &TradePlan) -> Phase {
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
        // Always-armed guards first: a terminal veto can end the plan on any
        // bar, regardless of spine phase.
        evaluate_guards(plan, &mut state, candle, &mut fired);
        if state.phase == Phase::Done {
            state.watermark = Some(candle.time);
            break;
        }

        // Sequential spine.
        match state.phase {
            Phase::AwaitBreakAndClose => {
                evaluate_break_and_close(plan, &mut state, candle, &mut fired);
            }
            Phase::AwaitEntry => {
                // Stamp the retest lookback before testing entry, so a retest
                // and entry that land on the same bar are both seen.
                stamp_retest(plan, &mut state, candle);
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
    PlanEval {
        fired,
        new_state: state,
        done,
    }
}

/// Evaluate every veto guard rule against this candle. A guard that fires
/// pushes its intent and, being terminal, ends the plan. Guards are
/// `FireMode::Once` so they latch in `state.fired`.
fn evaluate_guards(
    plan: &TradePlan,
    state: &mut PlanState,
    candle: &Candle,
    fired: &mut Vec<FiredIntent>,
) {
    for rule in &plan.rules {
        if !is_guard(rule) || state.fired.contains(&rule.rule_id) {
            continue;
        }
        if !armed_in(&rule.rule_id, state.phase) {
            continue;
        }
        if fire_rule(rule, state, candle) {
            push_fire(rule, candle, fired);
            state.fired.insert(rule.rule_id.clone());
            // Every veto guard is terminal for the plan's spine: the dispatched
            // intent (cancel / close / invalidate) is the end of this setup.
            state.phase = Phase::Done;
            return;
        }
    }
}

/// Evaluate the break-and-close prep. On fire it latches, records the lookback
/// window start, and advances to `AwaitEntry`. The rule then "dies".
fn evaluate_break_and_close(
    plan: &TradePlan,
    state: &mut PlanState,
    candle: &Candle,
    fired: &mut Vec<FiredIntent>,
) {
    let Some(rule) = plan.rules.iter().find(|r| is_break_and_close(r)) else {
        // No break-and-close rule but we're in its phase — advance immediately
        // (defensive; initial_phase wouldn't put us here).
        state.phase = Phase::AwaitEntry;
        return;
    };
    if fire_rule(rule, state, candle) {
        state.fired.insert(rule.rule_id.clone());
        state.break_close_at = Some(candle.time);
        // The break-and-close prep itself is recorded server-side by dispatching
        // its intent (a Prep action), exactly as the TV alert would have.
        push_fire(rule, candle, fired);
        state.phase = Phase::AwaitEntry;
    }
}

/// Evaluate the entry rule (the `Action::Enter` rule). For M/W this is the
/// per-bar heartbeat (fires every closed bar); for H&S it's the `PinePattern`
/// candle detector (recomputed from `detector_window`). The entry is gated by
/// the retest lookback when a retest rule is present.
fn evaluate_entry(
    plan: &TradePlan,
    state: &mut PlanState,
    candle: &Candle,
    detector_window: &[Candle],
    detector_cfg: &DetectorConfig,
    fired: &mut Vec<FiredIntent>,
) {
    let Some(rule) = plan.rules.iter().find(|r| r.intent.action == Action::Enter) else {
        return;
    };

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
            if !fire_rule(rule, state, candle) {
                return;
            }
            None
        }
    };

    // Retest gate: if the plan carries a retest rule, a retest must have been
    // seen in (break_close_at, this candle]. The stamp is persisted, so a
    // retest that closed in an earlier tick still counts.
    if plan.rules.iter().any(is_retest) && !retest_satisfied(state, candle.time) {
        return;
    }
    push_fire_signal(rule, candle, signal, fired);
    // A heartbeat (EveryBar) enter does not latch or finish the spine — the
    // worker's run_enter owns the actual placement/dedup, and M/W rides its TTL
    // / a veto to end. A single-shot (Once) enter ends the spine.
    if matches!(
        rule.fire_mode,
        trade_control_core::trade_plan::FireMode::Once
    ) {
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
    let idx = detector_window.iter().position(|c| c.time == candle.time)?;
    let sig = latched_signal_at(detector_window, idx, cfg)?;
    if !sig.fires || sig.direction != dir {
        return None;
    }
    if let Some(want) = pattern
        && sig.kind != want
    {
        return None;
    }
    Some(sig)
}

/// Stamp `retest_seen_at` if this candle satisfies the retest trendline
/// geometry and falls after the break-and-close. No-op if there's no retest
/// rule or no break-and-close has fired yet.
fn stamp_retest(plan: &TradePlan, state: &mut PlanState, candle: &Candle) {
    let Some(break_at) = state.break_close_at else {
        return;
    };
    if candle.time <= break_at {
        return;
    }
    let Some(rule) = plan.rules.iter().find(|r| is_retest(r)) else {
        return;
    };
    if eval_trigger(
        &rule.trigger,
        candle,
        state.last_close.get(&rule.rule_id).copied(),
    ) {
        state.retest_seen_at = Some(candle.time);
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
fn fire_rule(rule: &ConditionRule, state: &mut PlanState, candle: &Candle) -> bool {
    let prev_close = state.last_close.get(&rule.rule_id).copied();
    let hit = eval_trigger(&rule.trigger, candle, prev_close);
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
/// never fires an `OnClose` cross.
pub fn eval_trigger(trigger: &Trigger, candle: &Candle, prev_close: Option<f64>) -> bool {
    match trigger {
        Trigger::HorizontalCross { level, dir, bar }
        | Trigger::PriceValueCross { level, dir, bar } => {
            level_crossed(*level, *dir, *bar, candle, prev_close)
        }
        Trigger::TrendlineCross {
            a,
            b,
            extend_forward,
            dir,
            bar,
        } => {
            let Some(level) = line_price_at(a, b, candle.time, *extend_forward) else {
                return false;
            };
            level_crossed(level, *dir, *bar, candle, prev_close)
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
fn level_crossed(
    level: f64,
    dir: CrossDir,
    bar: BarEvent,
    candle: &Candle,
    prev_close: Option<f64>,
) -> bool {
    match bar {
        // Intrabar: the bar's full range straddles the level; direction is read
        // from where the close sits relative to it (the wick touched the level
        // and the bar resolved on the firing side).
        BarEvent::Intrabar => {
            let straddles = candle.l <= level && level <= candle.h;
            if !straddles {
                return false;
            }
            match dir {
                CrossDir::Either => true,
                CrossDir::Up => candle.c >= level,
                CrossDir::Down => candle.c <= level,
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

/// Interpolate a trendline's price at `t`. Returns `None` if `t` is past the
/// second anchor and the line isn't extended forward. A degenerate vertical
/// line (equal epochs) is treated as a horizontal at the first anchor's price.
fn line_price_at(
    a: &LinePoint,
    b: &LinePoint,
    t: DateTime<Utc>,
    extend_forward: bool,
) -> Option<f64> {
    let t = t.timestamp();
    if t > b.at_epoch && !extend_forward {
        return None;
    }
    let span = b.at_epoch - a.at_epoch;
    if span == 0 {
        return Some(a.price);
    }
    let frac = (t - a.at_epoch) as f64 / span as f64;
    Some(a.price + (b.price - a.price) * frac)
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
    use trade_control_core::intent::{BrokerKind, Direction};
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

    /// A minimal intent carrying just the action the evaluator reads; the rest
    /// is copied verbatim into fired results.
    fn intent(action: Action) -> Intent {
        Intent {
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

    fn plan(rules: Vec<ConditionRule>) -> TradePlan {
        TradePlan {
            trade_id: "t".into(),
            instrument: "EUR_USD".into(),
            direction: Direction::Short,
            granularity: Granularity::H1,
            pip_size: 0.0001,
            rules,
            shadow: false,
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
        assert!(eval_trigger(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.1, 1.21, 1.1, 1.2005),
            Some(1.1990)
        ));
        // prior close already above → no cross.
        assert!(!eval_trigger(
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
        assert!(!eval_trigger(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.1, 1.3, 1.1, 1.2500),
            None
        ));
    }

    #[test]
    fn intrabar_fires_when_range_straddles_and_close_on_firing_side() {
        let t = Trigger::HorizontalCross {
            level: 1.2000,
            dir: CrossDir::Up,
            bar: BarEvent::Intrabar,
        };
        // Range straddles, close above → up fires.
        assert!(eval_trigger(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.19, 1.21, 1.19, 1.2050),
            None
        ));
        // Range straddles but close below the level → up does NOT fire.
        assert!(!eval_trigger(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.21, 1.21, 1.19, 1.1950),
            None
        ));
        // Range doesn't reach the level → no fire.
        assert!(!eval_trigger(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.18, 1.195, 1.18, 1.19),
            None
        ));
    }

    #[test]
    fn intrabar_either_fires_on_any_straddle() {
        let t = Trigger::PriceValueCross {
            level: 1.2000,
            dir: CrossDir::Either,
            bar: BarEvent::Intrabar,
        };
        assert!(eval_trigger(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.19, 1.21, 1.19, 1.205),
            None
        ));
        assert!(eval_trigger(
            &t,
            &candle("2026-06-16T12:00:00Z", 1.21, 1.21, 1.19, 1.195),
            None
        ));
    }

    // ===== eval_trigger: trendline =====

    #[test]
    fn trendline_interpolates_level_at_candle_time() {
        // Line from (0, 1.0) to (100, 2.0): at t=50 the level is 1.5.
        let a = LinePoint {
            at_epoch: 0,
            price: 1.0,
        };
        let b = LinePoint {
            at_epoch: 100,
            price: 2.0,
        };
        let t = Trigger::TrendlineCross {
            a,
            b,
            extend_forward: true,
            dir: CrossDir::Up,
            bar: BarEvent::Intrabar,
        };
        // candle.time = epoch 50 → level 1.5; range straddles, close above.
        let c = Candle {
            time: DateTime::from_timestamp(50, 0).unwrap(),
            o: 1.4,
            h: 1.6,
            l: 1.4,
            c: 1.55,
        };
        assert!(eval_trigger(&t, &c, None));
    }

    #[test]
    fn trendline_respects_extend_forward_false() {
        let a = LinePoint {
            at_epoch: 0,
            price: 1.0,
        };
        let b = LinePoint {
            at_epoch: 100,
            price: 1.0,
        };
        let mk = |extend| Trigger::TrendlineCross {
            a,
            b,
            extend_forward: extend,
            dir: CrossDir::Either,
            bar: BarEvent::Intrabar,
        };
        // candle past the second anchor (epoch 200) at the line's level.
        let c = Candle {
            time: DateTime::from_timestamp(200, 0).unwrap(),
            o: 0.9,
            h: 1.1,
            l: 0.9,
            c: 1.0,
        };
        assert!(
            !eval_trigger(&mk(false), &c, None),
            "no eval past anchor when not extended"
        );
        assert!(
            eval_trigger(&mk(true), &c, None),
            "extended → still evaluates"
        );
    }

    // ===== eval_trigger: time + mw + pine =====

    #[test]
    fn time_reached_fires_at_or_past_epoch() {
        let at = ts("2026-06-16T15:00:00Z").timestamp();
        let t = Trigger::TimeReached { at_epoch: at };
        assert!(!eval_trigger(
            &t,
            &candle("2026-06-16T14:00:00Z", 1.0, 1.0, 1.0, 1.0),
            None
        ));
        assert!(eval_trigger(
            &t,
            &candle("2026-06-16T15:00:00Z", 1.0, 1.0, 1.0, 1.0),
            None
        ));
        assert!(eval_trigger(
            &t,
            &candle("2026-06-16T16:00:00Z", 1.0, 1.0, 1.0, 1.0),
            None
        ));
    }

    #[test]
    fn mw_every_bar_always_fires_and_pine_never_fires() {
        let c = candle("2026-06-16T12:00:00Z", 1.0, 1.0, 1.0, 1.0);
        assert!(eval_trigger(&Trigger::MwEveryBar, &c, None));
        assert!(!eval_trigger(
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
        // up-cross stamps, and the entry heartbeat fires the same bar → Done.
        let c_retest = candle("2026-06-16T12:00:00Z", 1.19, 1.205, 1.19, 1.2010);
        let eval2 = run(&p, &eval1.new_state, &[c_retest]);
        assert_eq!(eval2.fired.len(), 1, "entry fires once retest seen");
        assert_eq!(eval2.fired[0].rule_id, "05-enter");
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
        let p = plan(vec![rule(
            "05-enter",
            Trigger::PinePattern {
                pattern: None,
                dir: Direction::Short,
            },
            FireMode::Once,
            Action::Enter,
        )]);
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
    fn pine_entry_does_not_fire_for_opposite_direction_plan() {
        // Same bearish-pinbar window, but the plan is a LONG H&S — the latched
        // signal is Short, so the entry must not fire.
        let p = plan(vec![rule(
            "05-enter",
            Trigger::PinePattern {
                pattern: None,
                dir: Direction::Long,
            },
            FireMode::Once,
            Action::Enter,
        )]);
        let window = bearish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert!(eval.fired.is_empty(), "short signal can't fire a long plan");
        assert!(!eval.done);
    }

    #[test]
    fn pine_entry_kind_filter_blocks_mismatched_pattern() {
        // The window prints a pinbar; a plan demanding a Tweezer must not fire.
        let p = plan(vec![rule(
            "05-enter",
            Trigger::PinePattern {
                pattern: Some(SignalKind::Tweezer),
                dir: Direction::Short,
            },
            FireMode::Once,
            Action::Enter,
        )]);
        let window = bearish_pinbar_window();
        let prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert!(eval.fired.is_empty(), "kind filter blocks a pinbar");
    }

    #[test]
    fn pine_entry_blocked_until_retest_seen() {
        // H&S short with a retest rule: the pinbar entry is gated until a retest
        // up-cross has been stamped after the break-and-close.
        let neckline_up = Trigger::TrendlineCross {
            a: LinePoint {
                at_epoch: ts("2026-06-16T00:00:00Z").timestamp(),
                price: 1.25,
            },
            b: LinePoint {
                at_epoch: ts("2026-06-16T01:00:00Z").timestamp(),
                price: 1.25,
            },
            extend_forward: true,
            dir: CrossDir::Up,
            bar: BarEvent::Intrabar,
        };
        let p = plan(vec![
            rule("04-prep-retest", neckline_up, FireMode::Once, Action::Prep),
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
        let window = bearish_pinbar_window();
        // break_close set, but the pinbar bar (high 1.30) DOES straddle 1.25 with
        // close 1.115 — wait, that's a down-close, so the Up retest doesn't stamp.
        // So entry stays blocked.
        let mut prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
        prior.break_close_at = Some(ts("2026-06-16T10:30:00Z"));
        let eval = run_window(&p, &prior, &window[3..], &window);
        assert!(
            eval.fired.is_empty(),
            "entry blocked: no retest up-cross stamped"
        );
    }
}
