//! Fact-based retest tests for engine-v2.
//!
//! These drive a hand-built v2 [`TradePlan`] one bar at a time via `tick_once`
//! (through the `drive_series` caller-owns-loop helper) and assert on the
//! [`Facts`] blackboard. The retest is the first fact **consumer** — it gates on
//! the `("neckline","break_close")` fact a break-and-close rule produced, then
//! writes `("neckline","retest")`. The headline test drives BOTH rules through
//! the driver (the first two-rule producer/consumer chain), the rest exercise
//! the producer gate, the time-decaying tolerance, the fire-once guard, and the
//! spread-hour "rubbish candle" suppression.

use chrono::{DateTime, Utc};

use trade_control_core::broker::{Candle, Granularity};
use trade_control_core::intent::{Action, BrokerKind, Direction, Intent};
use trade_control_core::trade_plan::{BarEvent, CrossDir, LinePoint};
use trade_control_core::tunable::Tunable;

use trade_control_engine_v2::{
    Effect, EntryMechanism, FactValue, Facts, Line, PlanRule, PrepMap, RuleKind, TradePlan,
    tick_once,
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

fn candle_at(t: DateTime<Utc>, o: f64, h: f64, l: f64, c: f64) -> Candle {
    Candle {
        time: t,
        o,
        h,
        l,
        c,
    }
}

/// A minimal intent — only `action` / `instrument` matter for a prep; the rest
/// is copied verbatim into the fired result. `instrument` is set per-plan.
fn intent(instrument: &str) -> Intent {
    Intent {
        entry_level_vetos: Vec::new(),
        v: 1,
        id: "x".into(),
        not_before: None,
        not_after: ts("2026-06-30T00:00:00Z"),
        action: Action::Prep,
        instrument: instrument.into(),
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

/// A break-and-close rule (targets `Neckline`, the line the driver binds for
/// `RuleKind::BreakAndClose`).
fn bc_rule(instrument: &str, bar: BarEvent, dir: CrossDir) -> PlanRule {
    PlanRule {
        id: "03-prep-break-and-close".into(),
        kind: RuleKind::BreakAndClose,
        intent: intent(instrument),
        bar,
        dir,
        preps: PrepMap::new(),
        mechanism: EntryMechanism::Stop,
    }
}

/// A retest rule (targets `Neckline`, the line the driver binds for
/// `RuleKind::Retest`).
fn retest_rule(instrument: &str, bar: BarEvent, dir: CrossDir) -> PlanRule {
    PlanRule {
        id: "04-prep-retest".into(),
        kind: RuleKind::Retest,
        intent: intent(instrument),
        bar,
        dir,
        preps: PrepMap::new(),
        mechanism: EntryMechanism::Stop,
    }
}

/// A horizontal line at `price` (both anchors share the price), spanning
/// `[a_time, b_time]`. A horizontal keeps the tolerance maths trivial to reason
/// about (the level is constant regardless of bar-index).
fn horizontal_line(name: &str, price: f64, a_time: &str, b_time: &str) -> Line {
    Line {
        name: name.into(),
        a: LinePoint {
            at_epoch: ts(a_time).timestamp(),
            price,
        },
        b: LinePoint {
            at_epoch: ts(b_time).timestamp(),
            price,
        },
    }
}

fn plan(
    instrument: &str,
    lines: Vec<Line>,
    rules: Vec<PlanRule>,
    retest_atr_step: f64,
) -> TradePlan {
    TradePlan {
        trade_id: "t".into(),
        instrument: instrument.into(),
        direction: Direction::Long,
        granularity: Granularity::H1,
        lines,
        levels: Vec::new(),
        markers: Vec::new(),
        rules,
        cross_buffer_pct: 0.0,
        retest_atr_step,
    }
}

/// Drive a whole candle series one bar at a time (caller-owns-loop). Returns all
/// fires across the series (the retest emits none, but break-and-close does).
fn drive_series(
    plan: &TradePlan,
    facts: &mut Facts,
    candles: &[Candle],
    now: DateTime<Utc>,
) -> Vec<Effect> {
    let mut fires = Vec::new();
    for i in 0..candles.len() {
        // Prep rules are latest-bar-agnostic (they emit no acquisitive effect), so
        // every bar is driven as the latest bar here.
        fires.extend(tick_once(plan, facts, &candles[..=i], now, true));
    }
    fires
}

/// A warm ATR window of `n` H1 bars sitting just ABOVE the retest line at
/// 1.2000 — close 1.2010, high 1.2060, low 1.1960 → TR 0.010, flat closes. The
/// "just above" placement (vs the v1 helper's far-above 1.2100) keeps the warm
/// closes near the line so the later retest bars' true-range stays small and the
/// ATR the rule sees stays close to 0.010 — the tolerance boundary is then
/// stable enough to assert with a comfortable margin. None of these warmup bars'
/// lows reach 1.2000, so none is a retest.
fn warm_atr_window(n: usize, first_epoch: i64) -> Vec<Candle> {
    (0..n)
        .map(|i| {
            let t = first_epoch + (i as i64) * 3600;
            let ct = DateTime::from_timestamp(t, 0).expect("ts");
            Candle {
                time: ct,
                o: 1.2010,
                h: 1.2060,
                l: 1.1960,
                c: 1.2010,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// (1) Producer gate — no break_close fact ⇒ never stamps
// ---------------------------------------------------------------------------

/// A retest rule present, but NO break-and-close fact set (no b&c rule in the
/// plan) → a bar that WOULD cross the line does not stamp `retest`.
#[test]
fn retest_gated_on_producer_fact() {
    let ln = horizontal_line(
        "neckline",
        1.2000,
        "2026-06-01T09:00:00Z",
        "2026-06-01T20:00:00Z",
    );
    // Long retest = Down intrabar (a low reaching the line from above).
    let rr = retest_rule("EUR_USD", BarEvent::Intrabar, CrossDir::Down);
    let p = plan("EUR_USD", vec![ln], vec![rr], 0.075);

    let candles = vec![
        // A bar whose low dips to the line — would stamp IF the break was set.
        candle("2026-06-01T12:00:00Z", 1.2020, 1.2030, 1.1995, 1.2015),
        candle("2026-06-01T13:00:00Z", 1.2010, 1.2025, 1.1990, 1.2005),
    ];

    let mut facts = Facts::new();
    let _ = drive_series(&p, &mut facts, &candles, ts("2026-06-01T13:00:05Z"));

    assert!(
        !facts.is_set_named("neckline", "retest"),
        "no break_close fact ⇒ the retest never stamps, even on a genuine cross"
    );
}

// ---------------------------------------------------------------------------
// (2) First bar after the break — tolerance 0, must REACH the line
// ---------------------------------------------------------------------------

/// With the break stamped, the FIRST bar after it (N=1, tol 0) must actually
/// reach the line: a wick that falls short does NOT stamp; a wick that reaches
/// it does.
#[test]
fn first_bar_after_break_needs_to_reach_line() {
    let ln = horizontal_line(
        "neckline",
        1.2000,
        "2026-06-01T00:00:00Z",
        "2026-06-05T00:00:00Z",
    );
    let rr = retest_rule("EUR_USD", BarEvent::Intrabar, CrossDir::Down);
    let p = plan("EUR_USD", vec![ln], vec![rr], 0.075);

    // Case A: N=1 wick falls SHORT of the line (low 1.2005 > 1.2000) → no stamp.
    {
        let mut facts = Facts::new();
        facts.set_named(
            "neckline",
            "break_close",
            FactValue::At(ts("2026-06-01T12:00:00Z")),
        );
        // The bar strictly after the break, low short of the line.
        let candles = vec![candle(
            "2026-06-01T13:00:00Z",
            1.2020,
            1.2030,
            1.2005,
            1.2010,
        )];
        let _ = drive_series(&p, &mut facts, &candles, ts("2026-06-01T13:00:05Z"));
        assert!(
            !facts.is_set_named("neckline", "retest"),
            "N=1 tol 0: a wick short of the line must NOT stamp"
        );
    }

    // Case B: N=1 wick REACHES the line (low 1.1998 <= 1.2000) → stamp.
    {
        let mut facts = Facts::new();
        facts.set_named(
            "neckline",
            "break_close",
            FactValue::At(ts("2026-06-01T12:00:00Z")),
        );
        let candles = vec![candle(
            "2026-06-01T13:00:00Z",
            1.2020,
            1.2030,
            1.1998,
            1.2010,
        )];
        let _ = drive_series(&p, &mut facts, &candles, ts("2026-06-01T13:00:05Z"));
        assert_eq!(
            facts.at_named("neckline", "retest"),
            Some(ts("2026-06-01T13:00:00Z")),
            "N=1 tol 0: a wick reaching the line stamps at that bar's time"
        );
    }
}

// ---------------------------------------------------------------------------
// (3) Tolerance grows with bars-since-break
// ---------------------------------------------------------------------------

/// A later bar (N=2) stamps when its wick comes WITHIN the grown tolerance of
/// the line even without reaching it; an otherwise-identical wick that is NOT
/// within tolerance does not.
///
/// Warm window: 24 H1 bars @ TR 0.010 sitting just above the line → the ATR the
/// rule sees at the N=2 bar is ≈0.00933, so the N=2 tolerance is
/// `1 × 0.075 × 0.00933 ≈ 0.00070`, i.e. `line + tol ≈ 1.20070`. The two probe
/// wicks below sit either side of that boundary with a comfortable margin, so
/// small ATR-perturbation from the retest bars themselves can't flip the
/// assertions. The break is stamped on the last warmup bar; N=1 falls short,
/// N=2 is the tested bar. Line at 1.2000.
#[test]
fn tolerance_grows_with_bars_since_break() {
    let first = ts("2026-06-01T00:00:00Z").timestamp();
    // 24 warm bars → indices 0..23, last at first + 23*3600.
    let warm = warm_atr_window(24, first);
    let break_bar_epoch = first + 23 * 3600; // last warm bar's time
    let break_at = DateTime::from_timestamp(break_bar_epoch, 0).expect("ts");

    let ln = Line {
        name: "neckline".into(),
        a: LinePoint {
            at_epoch: first,
            price: 1.2000,
        },
        b: LinePoint {
            at_epoch: break_bar_epoch + 10 * 3600,
            price: 1.2000,
        },
    };
    let rr = retest_rule("EUR_USD", BarEvent::Intrabar, CrossDir::Down);
    let p = plan("EUR_USD", vec![ln], vec![rr], 0.075);

    // N=1 bar (tol 0): low 1.2003 — short of the line → no stamp.
    let n1 = candle_at(
        DateTime::from_timestamp(break_bar_epoch + 3600, 0).expect("ts"),
        1.2010,
        1.2020,
        1.2003,
        1.2010,
    );
    // N=2 tolerance ≈ 0.00070 → line + tol ≈ 1.20070. A low of 1.20050 is WITHIN
    // tol (well under 1.20070) but does NOT reach the line (1.20050 > 1.2000) →
    // stamps only because of the grown tolerance.
    let n2_within = candle_at(
        DateTime::from_timestamp(break_bar_epoch + 2 * 3600, 0).expect("ts"),
        1.2010,
        1.2020,
        1.20050,
        1.2010,
    );

    {
        let mut facts = Facts::new();
        facts.set_named("neckline", "break_close", FactValue::At(break_at));
        let mut series = warm.clone();
        series.push(n1);
        series.push(n2_within);
        let _ = drive_series(&p, &mut facts, &series, ts("2026-06-05T00:00:00Z"));
        assert_eq!(
            facts.at_named("neckline", "retest"),
            Some(n2_within.time),
            "N=2 within-tolerance wick stamps at the N=2 bar (N=1 fell short)"
        );
    }

    // Control: an N=2 wick just OUTSIDE tolerance (low 1.20080 > line+tol
    // ≈1.20070) does not stamp — proving the tolerance value, not merely "any
    // later bar stamps".
    {
        let mut facts = Facts::new();
        facts.set_named("neckline", "break_close", FactValue::At(break_at));
        let n2_outside = candle_at(
            DateTime::from_timestamp(break_bar_epoch + 2 * 3600, 0).expect("ts"),
            1.2010,
            1.2020,
            1.20080,
            1.2010,
        );
        let mut series = warm.clone();
        series.push(n1);
        series.push(n2_outside);
        let _ = drive_series(&p, &mut facts, &series, ts("2026-06-05T00:00:00Z"));
        assert!(
            !facts.is_set_named("neckline", "retest"),
            "N=2 wick beyond tolerance must NOT stamp"
        );
    }
}

// ---------------------------------------------------------------------------
// (4) Fire-once — once stamped, a later cross does not re-stamp
// ---------------------------------------------------------------------------

/// Once the retest stamps, a later crossing bar leaves the fact time unchanged.
#[test]
fn fire_once_prevents_restamp() {
    let ln = horizontal_line(
        "neckline",
        1.2000,
        "2026-06-01T00:00:00Z",
        "2026-06-05T00:00:00Z",
    );
    let rr = retest_rule("EUR_USD", BarEvent::Intrabar, CrossDir::Down);
    let p = plan("EUR_USD", vec![ln], vec![rr], 0.075);

    let mut facts = Facts::new();
    facts.set_named(
        "neckline",
        "break_close",
        FactValue::At(ts("2026-06-01T12:00:00Z")),
    );

    let candles = vec![
        // First retest: low reaches the line → stamps at 13:00.
        candle("2026-06-01T13:00:00Z", 1.2020, 1.2030, 1.1998, 1.2010),
        // Second, later cross that must NOT re-stamp (rule already done).
        candle("2026-06-01T15:00:00Z", 1.2020, 1.2030, 1.1990, 1.2010),
    ];
    let _ = drive_series(&p, &mut facts, &candles, ts("2026-06-01T15:00:05Z"));

    assert_eq!(
        facts.at_named("neckline", "retest"),
        Some(ts("2026-06-01T13:00:00Z")),
        "already done → stamp stays at the first cross, not re-stamped to 15:00"
    );
}

// ---------------------------------------------------------------------------
// (5) Spread-hour "rubbish candle" — cross does not stamp, scratch advances
// ---------------------------------------------------------------------------

/// A retest cross landing on a learned spread hour (EUR_USD 21:00 UTC in June /
/// EDT — a liquidity-vacuum wick) does NOT stamp `retest`; but because this rule
/// uses `OnClose`, the `last_close` scratch still advances so a genuine retest
/// on the next clean bar is measured correctly.
#[test]
fn spread_hour_cross_does_not_stamp_but_scratch_advances() {
    // OnClose Down retest so `trigger_uses_close` is true and `last_close` is
    // tracked. Line at 1.2000.
    let ln = horizontal_line(
        "neckline",
        1.2000,
        "2026-06-15T09:00:00Z",
        "2026-06-16T00:00:00Z",
    );
    let rr = retest_rule("EUR_USD", BarEvent::OnClose, CrossDir::Down);
    let p = plan("EUR_USD", vec![ln], vec![rr], 0.075);

    let mut facts = Facts::new();
    facts.set_named(
        "neckline",
        "break_close",
        FactValue::At(ts("2026-06-15T20:00:00Z")),
    );

    // Seed bar (20:00, NOT a spread hour) closes ABOVE the line → records
    // last_close = 1.2010, no cross.
    // Spread-hour bar (21:00) closes BELOW the line → a genuine OnClose down
    // cross, but suppressed because 21:00 UTC (June/EDT) is a spread hour.
    let candles = vec![
        candle("2026-06-15T20:00:00Z", 1.2015, 1.2020, 1.2005, 1.2010),
        candle("2026-06-15T21:00:00Z", 1.2008, 1.2009, 1.1985, 1.1990),
    ];
    let _ = drive_series(&p, &mut facts, &candles, ts("2026-06-15T21:00:05Z"));

    assert!(
        !facts.is_set_named("neckline", "retest"),
        "a retest cross on a spread hour is a rubbish candle — must NOT stamp"
    );
    // ...but the last_close scratch DID advance to the spread-hour bar's close,
    // so the next clean bar's OnClose cross measures correctly.
    assert_eq!(
        facts.num_scratch_named("04-prep-retest", "last_close"),
        Some(1.1990),
        "last_close scratch advances even on the suppressed spread-hour bar"
    );
}

// ---------------------------------------------------------------------------
// (6) Producer/consumer end-to-end — BOTH rules through the driver
// ---------------------------------------------------------------------------

/// The headline test: one plan with a break-and-close rule (first) AND a retest
/// rule (second). Drive a series where the neckline is broken-and-closed, then
/// retested. Both facts land, retest strictly after break_close. This is the
/// first two-rule chain through the driver — validation of the fact blackboard.
#[test]
fn break_and_close_then_retest_end_to_end() {
    // Neckline at 1.2000. Long trade: break-and-close DOWN through it (an iH&S
    // neckline breaks down before the long), then a retest (Down intrabar: the
    // low taps back to the neckline from below/at it) after the break.
    let ln = horizontal_line(
        "neckline",
        1.2000,
        "2026-06-01T00:00:00Z",
        "2026-06-05T00:00:00Z",
    );
    // b&c FIRST in plan.rules, retest SECOND — the baked producer→consumer order.
    let bc = bc_rule("EUR_USD", BarEvent::OnClose, CrossDir::Down);
    let rr = retest_rule("EUR_USD", BarEvent::Intrabar, CrossDir::Down);
    let p = plan("EUR_USD", vec![ln], vec![bc, rr], 0.075);

    // Kept short (4 bars) so the retest lands at N=1 (tol 0, no ATR needed) —
    // the whole producer→consumer chain is exercised without a warm window.
    let candles = vec![
        // Seed bar: closes above the line (records b&c last_close = 1.2010).
        candle("2026-06-01T10:00:00Z", 1.2012, 1.2015, 1.2008, 1.2010),
        // Break-and-close bar: closes 1.1990 below 1.2000 → stamps break_close.
        candle("2026-06-01T11:00:00Z", 1.2008, 1.2009, 1.1985, 1.1990),
        // Retest bar (N=1, the first bar strictly after the break): a Down
        // intrabar retest wants the LOW at/near the line, and at N=1 tol is 0 so
        // the low must REACH it. This bar straddles the line — low 1.1998 <=
        // 1.2000 <= high 1.2005 — so it stamps `retest`.
        candle("2026-06-01T12:00:00Z", 1.1995, 1.2005, 1.1998, 1.2001),
    ];

    let mut facts = Facts::new();
    let effects = drive_series(&p, &mut facts, &candles, ts("2026-06-01T12:00:05Z"));

    // The break-and-close rule fired once (the retest emits no Fire).
    assert_eq!(
        effects
            .iter()
            .filter(|e| matches!(e, Effect::Fire(_)))
            .count(),
        1,
        "only the break-and-close fires; the retest is fact-only"
    );

    let break_at = facts
        .at_named("neckline", "break_close")
        .expect("break_close fact must land");
    let retest_at = facts
        .at_named("neckline", "retest")
        .expect("retest fact must land");

    assert_eq!(
        break_at,
        ts("2026-06-01T11:00:00Z"),
        "break_close stamped at the down-close bar"
    );
    assert_eq!(
        retest_at,
        ts("2026-06-01T12:00:00Z"),
        "retest stamped at the tap-back bar"
    );
    assert!(
        retest_at > break_at,
        "the retest is strictly after the break-and-close"
    );
}
