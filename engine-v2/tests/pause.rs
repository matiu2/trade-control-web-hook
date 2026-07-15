//! Fact-based economic-news entry-pause tests for engine-v2.
//!
//! Drive a hand-built v2 [`TradePlan`] carrying `pause_windows` one bar at a time,
//! advancing the tick's `now` per bar (the pause rule tests membership against
//! wall-clock `now`, not `candle.time`), and assert on the plan-scoped `paused`
//! flag. Membership toggles: `Flag(true)` inside a window, cleared at the window's
//! end so the enter resumes.

use chrono::{DateTime, Utc};

use trade_control_core::broker::{Candle, Granularity};
use trade_control_core::intent::{Action, BrokerKind, Direction, Intent};
use trade_control_core::trade_plan::{BarEvent, CrossDir};
use trade_control_core::tunable::Tunable;

use trade_control_engine_v2::{
    Effect, EntryMechanism, Facts, NewsWindow, PlanRule, PrepMap, RuleKind, TradePlan, tick_once,
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

fn intent() -> Intent {
    Intent {
        entry_level_vetos: Vec::new(),
        v: 1,
        id: "x".into(),
        not_before: None,
        not_after: ts("2026-06-20T00:00:00Z"),
        action: Action::Pause,
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

/// The pause rule (reads plan-level pause_windows; no geometry).
fn pause_rule() -> PlanRule {
    PlanRule {
        id: "00-pause-news".into(),
        kind: RuleKind::Pause,
        intent: intent(),
        // bar/dir unused by the pause rule.
        bar: BarEvent::OnClose,
        dir: CrossDir::Up,
        preps: PrepMap::new(),
        mechanism: EntryMechanism::Stop,
    }
}

fn plan(pause_windows: Vec<NewsWindow>, rules: Vec<PlanRule>) -> TradePlan {
    TradePlan {
        trade_id: "t".into(),
        instrument: "EUR_USD".into(),
        direction: Direction::Short,
        granularity: Granularity::H1,
        lines: Vec::new(),
        levels: Vec::new(),
        markers: Vec::new(),
        pause_windows,
        rules,
        cross_buffer_pct: 0.0,
        retest_atr_step: trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
    }
}

/// Tick each bar with `now == candle.time` (the pause rule tests `now`, so the
/// bar's own time drives membership here — the realistic case where the tick fires
/// at the bar). Returns the `paused` flag after each bar.
fn drive_reading_flag(
    plan: &TradePlan,
    facts: &mut Facts,
    candles: &[Candle],
) -> Vec<Option<bool>> {
    let mut flags = Vec::new();
    for i in 0..candles.len() {
        let now = candles[i].time;
        tick_once(plan, facts, &candles[..=i], now, true);
        flags.push(facts.flag_named("__plan__", "paused"));
    }
    flags
}

// ---------------------------------------------------------------------------
// Core behaviour
// ---------------------------------------------------------------------------

/// The flag sets on entering a pause window and clears at its end (auto-resume).
/// Window [10:00, 14:00): bars at 09,10,12,14 → not/paused/paused/not.
#[test]
fn paused_flag_sets_inside_window_and_clears_at_end() {
    let win = NewsWindow::new(ts("2026-06-01T10:00:00Z"), ts("2026-06-01T14:00:00Z"));
    let p = plan(vec![win], vec![pause_rule()]);
    let mut facts = Facts::default();

    let candles = vec![
        candle("2026-06-01T09:00:00Z"), // before window
        candle("2026-06-01T10:00:00Z"), // at start → paused (start-inclusive)
        candle("2026-06-01T12:00:00Z"), // mid → paused
        candle("2026-06-01T14:00:00Z"), // at end → NOT paused (end-exclusive, resume)
    ];

    let flags = drive_reading_flag(&p, &mut facts, &candles);

    assert_eq!(
        flags,
        vec![
            // 09:00 outside: rule sees paused==flag(false-default) → no write → still unset.
            None,
            Some(true),  // 10:00 entered
            Some(true),  // 12:00 still in
            Some(false), // 14:00 window closed → cleared (auto-resume)
        ],
    );
}

/// Edge-triggered: the rule emits a `WriteFact` only when the state changes. The
/// driver applies `WriteFact` into `facts` without folding it into the returned
/// effect list, so this asserts at the rule level via [`Pause::tick`] directly —
/// a re-tick inside the window with the flag already set emits an empty vec.
#[test]
fn pause_is_edge_triggered_no_redundant_write() {
    use trade_control_engine_v2::{Pause, Rule, World};

    let win = NewsWindow::new(ts("2026-06-01T10:00:00Z"), ts("2026-06-01T14:00:00Z"));
    let p = plan(vec![win], vec![pause_rule()]);
    let rule = pause_rule();
    let inside = [candle("2026-06-01T10:00:00Z")];

    // Flag not yet set → entering the window emits one WriteFact.
    let mut facts = Facts::default();
    let world = World {
        now: ts("2026-06-01T10:00:00Z"),
        window: &inside,
        facts: &facts,
        plan: &p,
    };
    let e1 = Pause::new(&rule).tick(&world);
    assert_eq!(writes(&e1), 1, "entering the window emits one WriteFact");

    // Apply it, then re-tick inside the window: state unchanged → no effect.
    facts.set_named(
        "__plan__",
        "paused",
        trade_control_engine_v2::FactValue::Flag(true),
    );
    let world2 = World {
        now: ts("2026-06-01T11:00:00Z"),
        window: &inside,
        facts: &facts,
        plan: &p,
    };
    let e2 = Pause::new(&rule).tick(&world2);
    assert!(
        e2.is_empty(),
        "a further inside bar emits nothing (edge-triggered)"
    );
}

/// Multiple windows: the flag re-pauses for a second event after resuming from the
/// first. [10,12) then [16,18): paused, resume, paused, resume.
#[test]
fn multiple_windows_re_pause() {
    let w1 = NewsWindow::new(ts("2026-06-01T10:00:00Z"), ts("2026-06-01T12:00:00Z"));
    let w2 = NewsWindow::new(ts("2026-06-01T16:00:00Z"), ts("2026-06-01T18:00:00Z"));
    let p = plan(vec![w1, w2], vec![pause_rule()]);
    let mut facts = Facts::default();

    let candles = vec![
        candle("2026-06-01T11:00:00Z"), // in w1
        candle("2026-06-01T13:00:00Z"), // gap → resumed
        candle("2026-06-01T17:00:00Z"), // in w2 → paused again
        candle("2026-06-01T19:00:00Z"), // after w2 → resumed
    ];

    let flags = drive_reading_flag(&p, &mut facts, &candles);
    assert_eq!(
        flags,
        vec![Some(true), Some(false), Some(true), Some(false)],
        "flag re-pauses for the second event and clears after it",
    );
}

/// Count the `WriteFact` effects.
fn writes(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::WriteFact { .. }))
        .count()
}
