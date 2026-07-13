//! Parity harness: run hand-built break-and-close plans through BOTH the old
//! engine (`trade_control_engine::evaluate_plan`, the oracle) and the new
//! `trade_control_engine_v2::drive`, and assert the break-and-close-relevant
//! outputs are **byte-identical**:
//!
//! - `break_close_at` on `new_state`,
//! - the break-and-close `fired` entries (`rule_id`, `intent`, `candle`),
//! - `done`.
//!
//! Neither `PlanEval` nor `FiredIntent` implements `PartialEq` (the carried
//! `Intent` doesn't — see `core/src/plan_eval.rs`), so the comparison is on
//! **serialized JSON**, which is the right equality for float-bearing data and
//! exactly what the replay diff uses.
//!
//! # Why the comparison extracts a subset
//!
//! Slice 1 interprets only the break-and-close spine. The old engine also runs
//! controls, guards, retest, and entry each tick — so on a break-and-close-only
//! plan those arms produce nothing, but the two engines would legitimately
//! differ on *unrelated* state a later slice ports (e.g. the old engine seeds
//! `last_close` for a retest rule too). We therefore diff the parity-critical
//! break-and-close fields the TODO names, not the whole `PlanEval`. Every
//! fixture here is a break-and-close-only-relevant plan so this subset fully
//! captures the behaviour under test.

use chrono::{DateTime, Utc};
use serde_json::json;

use trade_control_core::broker::{Candle, Granularity};
use trade_control_core::intent::{Action, BrokerKind, Direction, Intent};
use trade_control_core::plan_eval::{FiredIntent, PlanEval};
use trade_control_core::plan_state::{Phase, PlanState};
use trade_control_core::trade_plan::{
    BarEvent, ConditionRule, CrossDir, DEFAULT_RETEST_ATR_STEP, FireMode, LinePoint, RuleKind,
    TradePlan, Trigger,
};
use trade_control_core::tunable::Tunable;

// ===== fixtures (mirror the old engine's test helpers) =====

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

/// A minimal intent carrying just the action the evaluator reads; the rest is
/// copied verbatim into fired results. Mirrors the old engine test's `intent()`.
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
        tick_size: None,
        spread_window: None,
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
        kind: RuleKind::Unspecified,
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
        cross_buffer_pct: 0.0,
        retest_atr_step: DEFAULT_RETEST_ATR_STEP,
        replay_start: None,
        armed_at: None,
        armed_sentiment: None,
    }
}

fn seed_at(phase: Phase, watermark: &str) -> PlanState {
    let mut s = PlanState::seed(phase, ts("2026-06-30T00:00:00Z"));
    s.watermark = Some(ts(watermark));
    s
}

/// A horizontal break-and-close plan: short closes DOWN through a fixed level,
/// `OnClose`. Simpler than a trendline (no bar-index interpolation) but exercises
/// the same `fire_rule` → `record_last_close` → stamp path.
fn horizontal_bc_plan(dir: CrossDir, bar: BarEvent, level: f64) -> TradePlan {
    plan(vec![rule(
        "03-prep-break-and-close",
        Trigger::HorizontalCross { level, dir, bar },
        FireMode::Once,
        Action::Prep,
    )])
}

/// A trendline break-and-close plan: neckline flat at 1.2000 over the window,
/// short closes DOWN through it. Exercises the bar-index line interpolation path.
fn trendline_bc_plan(dir: CrossDir, bar: BarEvent) -> TradePlan {
    let neckline = Trigger::TrendlineCross {
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
    plan(vec![rule(
        "03-prep-break-and-close",
        neckline,
        FireMode::Once,
        Action::Prep,
    )])
}

// ===== the diff: extract + compare the b&c-relevant subset =====

/// The break-and-close-relevant slice of a `PlanEval`, serialized to a stable
/// JSON value for byte-comparison. Extracts `break_close_at`, the `fired`
/// entries whose `rule_id` is the break-and-close prep (each as its full
/// serialized `FiredIntent` — rule_id, intent, candle, signal), and `done`.
fn bc_projection(eval: &PlanEval) -> serde_json::Value {
    let bc_fired: Vec<&FiredIntent> = eval
        .fired
        .iter()
        .filter(|f| f.rule_id.contains("prep-break-and-close"))
        .collect();
    json!({
        "break_close_at": eval.new_state.break_close_at,
        "fired": bc_fired,
        "done": eval.done,
    })
}

/// Run a plan through both engines and assert their break-and-close projections
/// are byte-identical. Returns the shared projection for extra assertions.
fn assert_parity(plan: &TradePlan, prior: &PlanState, candles: &[Candle]) -> serde_json::Value {
    let now = ts("2026-06-16T20:00:00Z");
    let expires = ts("2026-06-30T00:00:00Z");

    let old = trade_control_engine::evaluate_plan(plan, prior, candles, candles, now, expires);
    let new = trade_control_engine_v2::drive(plan, prior, candles, candles, now, expires);

    let old_p = bc_projection(&old);
    let new_p = bc_projection(&new);
    assert_eq!(
        serde_json::to_string(&old_p).unwrap(),
        serde_json::to_string(&new_p).unwrap(),
        "break-and-close projection diverged\n  old: {old_p}\n  new: {new_p}"
    );
    new_p
}

// ===== parity cases =====

/// OnClose break that fires and advances the phase: prior close above the
/// level, this close below → break-and-close stamps `break_close_at`, fires the
/// prep intent, and advances off `AwaitBreakAndClose`.
#[test]
fn onclose_break_fires_and_advances_phase() {
    let p = horizontal_bc_plan(CrossDir::Down, BarEvent::OnClose, 1.2000);
    let mut prior = seed_at(Phase::AwaitBreakAndClose, "2026-06-16T09:00:00Z");
    // Seed the prior close ABOVE the level so the OnClose cross has a prior.
    prior
        .last_close
        .insert("03-prep-break-and-close".into(), 1.2050);
    let c1 = candle("2026-06-16T10:00:00Z", 1.205, 1.205, 1.195, 1.1950);

    let proj = assert_parity(&p, &prior, &[c1]);

    // Sanity: the shared projection actually shows the fire + stamp (guards
    // against a vacuous "both did nothing identically" pass).
    assert_eq!(proj["break_close_at"], json!(ts("2026-06-16T10:00:00Z")));
    assert_eq!(proj["fired"].as_array().unwrap().len(), 1);
    assert_eq!(proj["done"], json!(false));
}

/// The trendline variant of the OnClose break — exercises the bar-index line
/// interpolation path, which the horizontal case skips.
#[test]
fn onclose_trendline_break_fires_identically() {
    let p = trendline_bc_plan(CrossDir::Down, BarEvent::OnClose);
    let mut prior = seed_at(Phase::AwaitBreakAndClose, "2026-06-16T09:00:00Z");
    prior
        .last_close
        .insert("03-prep-break-and-close".into(), 1.2050);
    // Window must carry the anchors + the crossing bar for bar-index resolution.
    let window = vec![
        candle("2026-06-16T00:00:00Z", 1.21, 1.21, 1.20, 1.205),
        candle("2026-06-16T01:00:00Z", 1.205, 1.21, 1.20, 1.205),
        candle("2026-06-16T10:00:00Z", 1.205, 1.205, 1.195, 1.1950),
    ];
    // Only the last bar is "new" (> watermark 09:00); pass the full window as
    // both new-candles and detector-window, matching the old engine's `run`.
    let new = &window[2..];

    let now = ts("2026-06-16T20:00:00Z");
    let expires = ts("2026-06-30T00:00:00Z");
    let old = trade_control_engine::evaluate_plan(&p, &prior, new, &window, now, expires);
    let new_eval = trade_control_engine_v2::drive(&p, &prior, new, &window, now, expires);
    assert_eq!(
        serde_json::to_string(&bc_projection(&old)).unwrap(),
        serde_json::to_string(&bc_projection(&new_eval)).unwrap(),
    );
    // And it really fired.
    assert_eq!(
        bc_projection(&new_eval)["break_close_at"],
        json!(ts("2026-06-16T10:00:00Z"))
    );
}

/// A bar that does NOT break: this close stays above the level, so no cross,
/// no stamp, no fire — both engines produce an empty projection identically.
#[test]
fn bar_that_does_not_break_stamps_nothing() {
    let p = horizontal_bc_plan(CrossDir::Down, BarEvent::OnClose, 1.2000);
    let mut prior = seed_at(Phase::AwaitBreakAndClose, "2026-06-16T09:00:00Z");
    prior
        .last_close
        .insert("03-prep-break-and-close".into(), 1.2050);
    // Close stays above the level → no down-cross.
    let c1 = candle("2026-06-16T10:00:00Z", 1.205, 1.206, 1.201, 1.2030);

    let proj = assert_parity(&p, &prior, &[c1]);
    assert_eq!(proj["break_close_at"], json!(null));
    assert!(proj["fired"].as_array().unwrap().is_empty());
    assert_eq!(proj["done"], json!(false));
}

/// The seed-bar-no-fire case: no prior close recorded for the rule (a fresh
/// plan on its first bar), so an OnClose cross can never fire even if the close
/// is below the level. Both engines agree: nothing stamps.
#[test]
fn seed_bar_no_prior_close_never_fires() {
    let p = horizontal_bc_plan(CrossDir::Down, BarEvent::OnClose, 1.2000);
    // No last_close seeded → prev_close is None → OnClose never fires.
    let prior = seed_at(Phase::AwaitBreakAndClose, "2026-06-16T09:00:00Z");
    // Close well below the level, but it's the seed bar for this rule.
    let c1 = candle("2026-06-16T10:00:00Z", 1.21, 1.21, 1.19, 1.1950);

    let proj = assert_parity(&p, &prior, &[c1]);
    assert_eq!(proj["break_close_at"], json!(null));
    assert!(proj["fired"].as_array().unwrap().is_empty());
}

/// Intrabar straddle break: an `Intrabar` break-and-close (low below the level,
/// high above → straddle) fires from the wick, close-agnostic. Exercises the
/// intrabar arm of `level_crossed` under parity.
#[test]
fn intrabar_straddle_break_fires_identically() {
    let p = horizontal_bc_plan(CrossDir::Down, BarEvent::Intrabar, 1.2000);
    let prior = seed_at(Phase::AwaitBreakAndClose, "2026-06-16T09:00:00Z");
    // Straddle: low 1.195 <= 1.20 <= high 1.205; low reached below → Down fires.
    // Intrabar reads no prior close, so no last_close seed is needed.
    let c1 = candle("2026-06-16T10:00:00Z", 1.204, 1.205, 1.195, 1.2030);

    let proj = assert_parity(&p, &prior, &[c1]);
    assert_eq!(proj["break_close_at"], json!(ts("2026-06-16T10:00:00Z")));
    assert_eq!(proj["fired"].as_array().unwrap().len(), 1);
}

/// A bar whose range never reaches the level: no straddle → the intrabar break
/// does not fire. Parity on the negative intrabar path.
#[test]
fn intrabar_no_straddle_does_not_fire() {
    let p = horizontal_bc_plan(CrossDir::Down, BarEvent::Intrabar, 1.2000);
    let prior = seed_at(Phase::AwaitBreakAndClose, "2026-06-16T09:00:00Z");
    // Range 1.201..1.206 stays entirely above the level → no straddle.
    let c1 = candle("2026-06-16T10:00:00Z", 1.205, 1.206, 1.201, 1.2030);

    let proj = assert_parity(&p, &prior, &[c1]);
    assert_eq!(proj["break_close_at"], json!(null));
    assert!(proj["fired"].as_array().unwrap().is_empty());
}

/// A multi-tick run: the break lands on the *second* new candle, and the first
/// (non-breaking) candle must seed `last_close` so the second's OnClose cross is
/// measured against it. Exercises the driver's per-candle loop + the
/// record-before-gate bookkeeping under parity.
#[test]
fn break_on_second_candle_uses_seeded_last_close() {
    let p = horizontal_bc_plan(CrossDir::Down, BarEvent::OnClose, 1.2000);
    let mut prior = seed_at(Phase::AwaitBreakAndClose, "2026-06-16T09:00:00Z");
    prior
        .last_close
        .insert("03-prep-break-and-close".into(), 1.2100);
    // Bar 1: closes above the level (no cross) but seeds last_close = 1.2050.
    let c1 = candle("2026-06-16T10:00:00Z", 1.21, 1.211, 1.204, 1.2050);
    // Bar 2: closes below the level → down-cross vs the 1.2050 prior close.
    let c2 = candle("2026-06-16T11:00:00Z", 1.205, 1.205, 1.195, 1.1950);

    let proj = assert_parity(&p, &prior, &[c1, c2]);
    // The stamp is the SECOND bar's time.
    assert_eq!(proj["break_close_at"], json!(ts("2026-06-16T11:00:00Z")));
    assert_eq!(proj["fired"].as_array().unwrap().len(), 1);
}

/// `seed_plan_state` parity: a fresh plan seeds an identical initial phase,
/// watermark, and break-and-close `last_close` in both engines (so the *next*
/// tick's OnClose cross is measured against the same prior close).
#[test]
fn seed_plan_state_matches_old_engine() {
    let p = horizontal_bc_plan(CrossDir::Down, BarEvent::OnClose, 1.2000);
    let window = vec![
        candle("2026-06-16T08:00:00Z", 1.21, 1.212, 1.208, 1.2100),
        candle("2026-06-16T09:00:00Z", 1.21, 1.211, 1.204, 1.2050),
    ];
    let expires = ts("2026-06-30T00:00:00Z");

    let old = trade_control_engine::seed_plan_state(&p, &window, expires);
    let new = trade_control_engine_v2::seed_plan_state(&p, &window, expires);

    // The break-and-close-relevant seed fields must match byte-for-byte.
    assert_eq!(old.phase, new.phase);
    assert_eq!(old.watermark, new.watermark);
    assert_eq!(
        old.last_close.get("03-prep-break-and-close"),
        new.last_close.get("03-prep-break-and-close"),
    );
    // And it seeded the newest bar's close (1.2050) for the OnClose rule.
    assert_eq!(new.watermark, Some(ts("2026-06-16T09:00:00Z")));
    assert_eq!(new.last_close.get("03-prep-break-and-close"), Some(&1.2050));
}

/// `initial_phase` parity for a break-and-close plan (gated) and a plan without
/// one (straight to entry).
#[test]
fn initial_phase_matches_old_engine() {
    let with_bc = horizontal_bc_plan(CrossDir::Down, BarEvent::OnClose, 1.2000);
    assert_eq!(
        trade_control_engine::initial_phase(&with_bc),
        trade_control_engine_v2::initial_phase(&with_bc),
    );
    assert_eq!(
        trade_control_engine_v2::initial_phase(&with_bc),
        Phase::AwaitBreakAndClose
    );

    // A plan with no break-and-close rule (just a lone prep) starts at entry.
    let no_bc = plan(vec![rule(
        "04-prep-retest",
        Trigger::HorizontalCross {
            level: 1.2000,
            dir: CrossDir::Up,
            bar: BarEvent::Intrabar,
        },
        FireMode::Once,
        Action::Prep,
    )]);
    assert_eq!(
        trade_control_engine::initial_phase(&no_bc),
        trade_control_engine_v2::initial_phase(&no_bc),
    );
    assert_eq!(
        trade_control_engine_v2::initial_phase(&no_bc),
        Phase::AwaitEntry
    );
}

/// Idempotence / latch: a plan whose break-and-close has ALREADY fired (latched
/// in `state.fired`, `break_close_at` stamped, phase `AwaitEntry`) must not
/// re-stamp on a later re-cross. Both engines honour the latch identically.
#[test]
fn already_fired_break_and_close_does_not_restamp() {
    let p = horizontal_bc_plan(CrossDir::Down, BarEvent::OnClose, 1.2000);
    // Prior: already past break-and-close, in AwaitEntry, stamped at 10:00.
    let mut prior = seed_at(Phase::AwaitEntry, "2026-06-16T10:00:00Z");
    prior.fired.insert("03-prep-break-and-close".into());
    prior.break_close_at = Some(ts("2026-06-16T10:00:00Z"));
    prior
        .last_close
        .insert("03-prep-break-and-close".into(), 1.1950);
    // A later bar that would re-cross down — must NOT re-stamp.
    let c1 = candle("2026-06-16T11:00:00Z", 1.201, 1.201, 1.195, 1.1960);

    let proj = assert_parity(&p, &prior, &[c1]);
    // Stamp stays at the original 10:00, no new break-and-close fire.
    assert_eq!(proj["break_close_at"], json!(ts("2026-06-16T10:00:00Z")));
    assert!(proj["fired"].as_array().unwrap().is_empty());
}
