//! Fact-based invalidation-cap tests for engine-v2.
//!
//! Drive a hand-built v2 [`TradePlan`] carrying a `PriceLevel` cap one bar at a
//! time via `tick_once` and assert on the plan-scoped retire fact + the returned
//! [`Effect::Invalidate`]. Exercises the whole 4c path end-to-end: the
//! `PriceLevel` model, the no-projection `eval_level` cross, and the driver's
//! terminal retire — the parts the earlier steps unit-tested in isolation.

use chrono::{DateTime, Utc};

use trade_control_core::broker::{Candle, Granularity};
use trade_control_core::intent::{Action, BrokerKind, Direction, Intent};
use trade_control_core::trade_plan::{BarEvent, CrossDir};
use trade_control_core::tunable::Tunable;

use trade_control_engine_v2::{
    Effect, EntryMechanism, Facts, PlanRule, PrepMap, PriceLevel, RuleKind, TradePlan, tick_once,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .expect("valid rfc3339")
        .with_timezone(&Utc)
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

/// A minimal intent — only `instrument` matters for the spread-hour gate; the
/// rest is copied verbatim into any fired result (an invalidate emits none).
fn intent() -> Intent {
    Intent {
        entry_level_vetos: Vec::new(),
        v: 1,
        id: "x".into(),
        not_before: None,
        not_after: ts("2026-06-20T00:00:00Z"),
        action: Action::Veto,
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

/// An invalidation-cap rule. `kind` picks the cap (`InvalidateHigh` → `too_high`,
/// `InvalidateLow` → `too_low`), which the driver binds to the matching level.
fn cap_rule(id: &str, kind: RuleKind, bar: BarEvent, dir: CrossDir) -> PlanRule {
    PlanRule {
        id: id.into(),
        kind,
        intent: intent(),
        bar,
        dir,
        preps: PrepMap::new(),
        mechanism: EntryMechanism::Stop,
    }
}

/// A plan carrying `levels` and `rules` — no lines (the caps are the whole test).
fn plan(levels: Vec<PriceLevel>, rules: Vec<PlanRule>) -> TradePlan {
    TradePlan {
        trade_id: "t".into(),
        instrument: "EUR_USD".into(),
        direction: Direction::Short,
        granularity: Granularity::H1,
        lines: Vec::new(),
        levels,
        rules,
        cross_buffer_pct: 0.0,
        retest_atr_step: trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
    }
}

/// Drive a candle series one bar at a time, as the worker/replay will. Returns
/// all effects the driver folded into its returned list across the series (fires
/// + any `Invalidate`).
fn drive_series(
    plan: &TradePlan,
    facts: &mut Facts,
    candles: &[Candle],
    now: DateTime<Utc>,
) -> Vec<Effect> {
    let mut out = Vec::new();
    for i in 0..candles.len() {
        out.extend(tick_once(plan, facts, &candles[..=i], now, true));
    }
    out
}

/// Count the `Invalidate` effects.
fn invalidates(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::Invalidate { .. }))
        .count()
}

const NOW: &str = "2026-06-30T00:00:00Z";

// ---------------------------------------------------------------------------
// Core behaviour
// ---------------------------------------------------------------------------

/// A bar whose high pierces the upper cap (short's `too_high`) intrabar-Up →
/// retires the plan: stamps `(__plan__, invalidated)` and returns one
/// `Effect::Invalidate`.
#[test]
fn too_high_cross_retires_the_plan() {
    let levels = vec![PriceLevel {
        name: "too_high".into(),
        price: 1.1050,
    }];
    let rule = cap_rule(
        "01-veto-too-high",
        RuleKind::InvalidateHigh,
        BarEvent::Intrabar,
        CrossDir::Up,
    );
    let p = plan(levels, vec![rule]);
    let mut facts = Facts::default();

    // A bar entirely below the cap: no retire.
    let below = candle("2026-06-01T09:00:00Z", 1.100, 1.104, 1.099, 1.103);
    // A bar whose high pierces 1.1050: retire.
    let cross = candle("2026-06-01T10:00:00Z", 1.103, 1.106, 1.102, 1.104);

    let out = drive_series(&p, &mut facts, &[below, cross], ts(NOW));

    assert_eq!(invalidates(&out), 1, "one retire on the piercing bar");
    assert!(
        facts.is_set_named("__plan__", "invalidated"),
        "the plan-scoped retire fact is stamped",
    );
    assert_eq!(
        facts.at_named("__plan__", "invalidated"),
        Some(ts(NOW)),
        "retire fact stamped at the tick's now",
    );
}

/// Fire-once: once retired, a second cap cross does not retire again (the
/// plan-scoped guard).
#[test]
fn retire_is_fire_once() {
    let levels = vec![PriceLevel {
        name: "too_high".into(),
        price: 1.1050,
    }];
    let rule = cap_rule(
        "01-veto-too-high",
        RuleKind::InvalidateHigh,
        BarEvent::Intrabar,
        CrossDir::Up,
    );
    let p = plan(levels, vec![rule]);
    let mut facts = Facts::default();

    // Two bars that both pierce the cap.
    let cross1 = candle("2026-06-01T10:00:00Z", 1.103, 1.106, 1.102, 1.104);
    let cross2 = candle("2026-06-01T11:00:00Z", 1.104, 1.107, 1.103, 1.105);

    let out = drive_series(&p, &mut facts, &[cross1, cross2], ts(NOW));

    assert_eq!(
        invalidates(&out),
        1,
        "only the first cap cross retires; the second is a no-op (fire-once)",
    );
}

/// A bar that stays below the cap never retires.
#[test]
fn below_cap_does_not_retire() {
    let levels = vec![PriceLevel {
        name: "too_high".into(),
        price: 1.1050,
    }];
    let rule = cap_rule(
        "01-veto-too-high",
        RuleKind::InvalidateHigh,
        BarEvent::Intrabar,
        CrossDir::Up,
    );
    let p = plan(levels, vec![rule]);
    let mut facts = Facts::default();

    let b1 = candle("2026-06-01T09:00:00Z", 1.100, 1.1040, 1.099, 1.103);
    let b2 = candle("2026-06-01T10:00:00Z", 1.103, 1.1049, 1.102, 1.104);

    let out = drive_series(&p, &mut facts, &[b1, b2], ts(NOW));

    assert_eq!(invalidates(&out), 0, "no bar reaches the cap → no retire");
    assert!(
        !facts.is_set_named("__plan__", "invalidated"),
        "plan not retired",
    );
}

/// The lower cap (`too_low`, `InvalidateLow`) retires on a `Down` cross — the
/// mirror of the upper cap, driven by the second `RuleKind` arm.
#[test]
fn too_low_cross_retires_via_invalidate_low() {
    let levels = vec![PriceLevel {
        name: "too_low".into(),
        price: 1.0950,
    }];
    let rule = cap_rule(
        "01-veto-too-low",
        RuleKind::InvalidateLow,
        BarEvent::Intrabar,
        CrossDir::Down,
    );
    let p = plan(levels, vec![rule]);
    let mut facts = Facts::default();

    // A bar whose low pierces below 1.0950.
    let cross = candle("2026-06-01T10:00:00Z", 1.098, 1.099, 1.094, 1.096);

    let out = drive_series(&p, &mut facts, &[cross], ts(NOW));

    assert_eq!(
        invalidates(&out),
        1,
        "the lower cap retires on a Down cross"
    );
    assert!(facts.is_set_named("__plan__", "invalidated"));
}

/// The `cross_buffer_pct` buffer applies: a graze that reaches the bare cap but
/// not past the buffer does not retire.
#[test]
fn buffer_suppresses_a_graze() {
    let levels = vec![PriceLevel {
        name: "too_high".into(),
        price: 1.1050,
    }];
    let rule = cap_rule(
        "01-veto-too-high",
        RuleKind::InvalidateHigh,
        BarEvent::Intrabar,
        CrossDir::Up,
    );
    // Buffer 0.1% of 1.1050 ≈ 0.0011, so an Up cross needs high ≥ ~1.1061.
    let mut p = plan(levels, vec![rule]);
    p.cross_buffer_pct = 0.1;
    let mut facts = Facts::default();

    // Grazes to exactly the bare cap (high 1.1050) — inside the buffer, no retire.
    let graze = candle("2026-06-01T10:00:00Z", 1.104, 1.1050, 1.103, 1.1049);
    let out = drive_series(&p, &mut facts, &[graze], ts(NOW));
    assert_eq!(
        invalidates(&out),
        0,
        "a graze inside the buffer does not retire"
    );
    assert!(!facts.is_set_named("__plan__", "invalidated"));
}
