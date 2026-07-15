//! Fact-based trade-expiry tests for engine-v2.
//!
//! Drive a hand-built v2 [`TradePlan`] carrying a `TimeMarker` (expiry) one bar at
//! a time and assert on the plan-scoped retire fact + the returned
//! [`Effect::Invalidate`]. Exercises the whole 4d path end-to-end: the
//! `TimeMarker` model, the no-price `eval_time` cross, and the reuse of 4c's
//! terminal retire (expiry *is* a retirement).

use chrono::{DateTime, Utc};

use trade_control_core::broker::{Candle, Granularity};
use trade_control_core::intent::{Action, BrokerKind, Direction, Intent};
use trade_control_core::trade_plan::{BarEvent, CrossDir};
use trade_control_core::tunable::Tunable;

use trade_control_engine_v2::{
    Effect, EntryMechanism, Facts, PlanRule, PrepMap, RuleKind, TimeMarker, TradePlan, tick_once,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .expect("valid rfc3339")
        .with_timezone(&Utc)
}

fn candle(time: &str) -> Candle {
    Candle {
        time: ts(time),
        o: 1.0,
        h: 1.0,
        l: 1.0,
        c: 1.0,
    }
}

/// A minimal intent — only `instrument` is read (and nothing gates expiry on it).
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

/// A trade-expiry rule (bound to the `Expiry` marker by `RuleKind::Expiry`).
fn expiry_rule() -> PlanRule {
    PlanRule {
        id: "02-veto-trade-expiry".into(),
        kind: RuleKind::Expiry,
        intent: intent(),
        // bar/dir are unused by the expiry rule (a time check, no price cross) —
        // set to any value.
        bar: BarEvent::OnClose,
        dir: CrossDir::Up,
        preps: PrepMap::new(),
        mechanism: EntryMechanism::Stop,
    }
}

/// A no-prep enter rule (`Action::Enter`) — places immediately once it ticks
/// unless the plan is retired. Used by the end-to-end block test.
fn enter_rule() -> PlanRule {
    let mut i = intent();
    i.action = Action::Enter;
    PlanRule {
        id: "05-enter".into(),
        kind: RuleKind::Enter,
        intent: i,
        bar: BarEvent::OnClose,
        dir: CrossDir::Up,
        preps: PrepMap::new(),
        mechanism: EntryMechanism::Stop,
    }
}

/// A plan carrying `markers` and `rules`.
fn plan(markers: Vec<TimeMarker>, rules: Vec<PlanRule>) -> TradePlan {
    TradePlan {
        trade_id: "t".into(),
        instrument: "EUR_USD".into(),
        direction: Direction::Short,
        granularity: Granularity::H1,
        lines: Vec::new(),
        levels: Vec::new(),
        markers,
        rules,
        cross_buffer_pct: 0.0,
        retest_atr_step: trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
    }
}

/// An `expiry` marker at the given RFC3339 time.
fn expiry_marker(at: &str) -> TimeMarker {
    TimeMarker {
        name: "expiry".into(),
        at_epoch: ts(at).timestamp(),
    }
}

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

fn invalidates(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::Invalidate { .. }))
        .count()
}

fn place_orders(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::PlaceOrder { .. }))
        .count()
}

const NOW: &str = "2026-06-30T00:00:00Z";

// ---------------------------------------------------------------------------
// Core behaviour
// ---------------------------------------------------------------------------

/// When the bar reaches the expiry time the plan retires: stamps `(__plan__,
/// invalidated)` and returns one `Effect::Invalidate`. A bar before expiry does
/// nothing.
#[test]
fn expiry_retires_when_bar_reaches_marker() {
    let p = plan(
        vec![expiry_marker("2026-06-01T12:00:00Z")],
        vec![expiry_rule()],
    );
    let mut facts = Facts::default();

    let before = candle("2026-06-01T11:00:00Z");
    let at = candle("2026-06-01T12:00:00Z");

    let out = drive_series(&p, &mut facts, &[before, at], ts(NOW));

    assert_eq!(
        invalidates(&out),
        1,
        "one retire when the bar reaches expiry"
    );
    assert!(
        facts.is_set_named("__plan__", "invalidated"),
        "the plan-scoped retire fact is stamped",
    );
}

/// Fire-once: once expired, a later bar does not retire again.
#[test]
fn expiry_is_fire_once() {
    let p = plan(
        vec![expiry_marker("2026-06-01T12:00:00Z")],
        vec![expiry_rule()],
    );
    let mut facts = Facts::default();

    let at = candle("2026-06-01T12:00:00Z");
    let after = candle("2026-06-01T13:00:00Z");

    let out = drive_series(&p, &mut facts, &[at, after], ts(NOW));

    assert_eq!(
        invalidates(&out),
        1,
        "only the first past-expiry bar retires; later bars are no-ops",
    );
}

/// A bar before expiry never retires.
#[test]
fn before_expiry_does_not_retire() {
    let p = plan(
        vec![expiry_marker("2026-06-01T12:00:00Z")],
        vec![expiry_rule()],
    );
    let mut facts = Facts::default();

    let b1 = candle("2026-06-01T09:00:00Z");
    let b2 = candle("2026-06-01T11:00:00Z");

    let out = drive_series(&p, &mut facts, &[b1, b2], ts(NOW));

    assert_eq!(invalidates(&out), 0, "no bar reaches expiry → no retire");
    assert!(!facts.is_set_named("__plan__", "invalidated"));
}

/// End-to-end: a plan with an expiry marker **and** a no-prep enter. On the bar
/// that reaches expiry the plan retires (expiry rule ordered first), and the enter
/// — which would otherwise place immediately — is blocked by its retire-fact guard
/// on the same bar. No `PlaceOrder` ever leaves.
#[test]
fn expiry_blocks_the_enter_end_to_end() {
    // Expiry FIRST, enter second — within a bar the driver applies the retire fact
    // before the enter ticks.
    let p = plan(
        vec![expiry_marker("2026-06-01T12:00:00Z")],
        vec![expiry_rule(), enter_rule()],
    );
    let mut facts = Facts::default();

    let at = candle("2026-06-01T12:00:00Z");
    let out = drive_series(&p, &mut facts, &[at], ts(NOW));

    assert_eq!(invalidates(&out), 1, "expiry retires the plan");
    assert_eq!(
        place_orders(&out),
        0,
        "the expired plan blocks the enter on the very bar it expired",
    );
}
