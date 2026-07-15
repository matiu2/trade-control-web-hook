//! Fact-based break-and-close tests for engine-v2.
//!
//! These drive a hand-built v2 [`TradePlan`] one bar at a time via
//! `tick_once` (through the `drive_series` caller-owns-loop test helper) and
//! assert on the [`Facts`] blackboard + returned [`Effect`]s — the fact-based
//! contract, no `Phase`. The "known-hard" cases (sloped neckline, weekend gap,
//! forward projection past the second anchor) prove the reused `cross.rs`
//! projection still works in the v2 line shape.

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

/// A minimal intent — only `action` / `instrument` matter for a prep; the rest
/// is copied verbatim into the fired result.
fn intent() -> Intent {
    Intent {
        entry_level_vetos: Vec::new(),
        v: 1,
        id: "x".into(),
        not_before: None,
        not_after: ts("2026-06-20T00:00:00Z"),
        action: Action::Prep,
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

/// A break-and-close rule (targets `Neckline`, the line the driver binds for
/// `RuleKind::BreakAndClose`).
fn bc_rule(bar: BarEvent, dir: CrossDir) -> PlanRule {
    PlanRule {
        id: "03-prep-break-and-close".into(),
        kind: RuleKind::BreakAndClose,
        intent: intent(),
        bar,
        dir,
        preps: PrepMap::new(),
        mechanism: EntryMechanism::Stop,
    }
}

/// A horizontal "neckline" line at `price` (both anchors share the price),
/// spanning `[a_time, b_time]`.
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

fn plan(lines: Vec<Line>, rules: Vec<PlanRule>) -> TradePlan {
    TradePlan {
        trade_id: "t".into(),
        instrument: "EUR_USD".into(),
        direction: Direction::Short,
        granularity: Granularity::H1,
        lines,
        levels: Vec::new(),
        rules,
        cross_buffer_pct: 0.0,
        retest_atr_step: trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
    }
}

/// Count the `Fire` effects.
fn fires(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::Fire(_)))
        .count()
}

/// Drive a whole candle series one bar at a time (caller-owns-loop), the way
/// the live worker / replay will. Returns all fires across the series.
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

// ---------------------------------------------------------------------------
// Core fact-based behaviour
// ---------------------------------------------------------------------------

/// A candle that closes DOWN through the neckline (short, OnClose, Down) →
/// stamps `("neckline","break_close") = At(candle.time)` and fires once.
#[test]
fn onclose_cross_stamps_fact_and_fires() {
    // Neckline at 1.1000. Seed bar closes above (1.1010); next bar closes below.
    let ln = horizontal_line(
        "neckline",
        1.1000,
        "2026-06-01T09:00:00Z",
        "2026-06-01T20:00:00Z",
    );
    let rule = bc_rule(BarEvent::OnClose, CrossDir::Down);
    let p = plan(vec![ln], vec![rule]);

    let candles = vec![
        // Seed bar: sits above the line, records last_close = 1.1010.
        candle("2026-06-01T12:00:00Z", 1.1012, 1.1015, 1.1008, 1.1010),
        // Cross bar: closes at 1.0990, below 1.1000 → genuine down close-through.
        candle("2026-06-01T13:00:00Z", 1.1008, 1.1009, 1.0985, 1.0990),
    ];

    let mut facts = Facts::new();
    let effects = drive_series(&p, &mut facts, &candles, ts("2026-06-01T13:00:05Z"));

    assert_eq!(fires(&effects), 1, "expected exactly one fire");
    assert_eq!(
        facts.at_named("neckline", "break_close"),
        Some(ts("2026-06-01T13:00:00Z")),
        "break_close fact stamped to the cross candle's time"
    );
}

/// A candle that does NOT cross the neckline → no fact, no fire.
#[test]
fn no_cross_sets_no_fact_and_no_fire() {
    let ln = horizontal_line(
        "neckline",
        1.1000,
        "2026-06-01T09:00:00Z",
        "2026-06-01T20:00:00Z",
    );
    let rule = bc_rule(BarEvent::OnClose, CrossDir::Down);
    let p = plan(vec![ln], vec![rule]);

    let candles = vec![
        candle("2026-06-01T12:00:00Z", 1.1012, 1.1015, 1.1008, 1.1010),
        // Stays above the line — closes 1.1005, never below 1.1000.
        candle("2026-06-01T13:00:00Z", 1.1010, 1.1014, 1.1004, 1.1005),
    ];

    let mut facts = Facts::new();
    let effects = drive_series(&p, &mut facts, &candles, ts("2026-06-01T13:00:05Z"));

    assert_eq!(fires(&effects), 0, "no cross → no fire");
    assert!(
        !facts.is_set_named("neckline", "break_close"),
        "no cross → no break_close fact"
    );
}

/// Fire-once: once fired, a later crossing candle must not re-fire or re-stamp.
#[test]
fn fire_once_prevents_refire_and_restamp() {
    let ln = horizontal_line(
        "neckline",
        1.1000,
        "2026-06-01T09:00:00Z",
        "2026-06-02T20:00:00Z",
    );
    let rule = bc_rule(BarEvent::OnClose, CrossDir::Down);
    let p = plan(vec![ln], vec![rule]);

    let candles = vec![
        candle("2026-06-01T12:00:00Z", 1.1012, 1.1015, 1.1008, 1.1010),
        // First cross → fires, stamps at 13:00.
        candle("2026-06-01T13:00:00Z", 1.1008, 1.1009, 1.0985, 1.0990),
        // Price pops back above the line...
        candle("2026-06-01T14:00:00Z", 1.0992, 1.1013, 1.0990, 1.1010),
        // ...and closes back below — a second genuine down-cross that must be
        // ignored because the rule already fired (fire-once).
        candle("2026-06-01T15:00:00Z", 1.1008, 1.1009, 1.0980, 1.0985),
    ];

    let mut facts = Facts::new();
    let effects = drive_series(&p, &mut facts, &candles, ts("2026-06-01T15:00:05Z"));

    assert_eq!(fires(&effects), 1, "fire-once → only the first cross fires");
    assert_eq!(
        facts.at_named("neckline", "break_close"),
        Some(ts("2026-06-01T13:00:00Z")),
        "stamp stays at the first cross's time, not re-stamped to 15:00"
    );
}

// ---------------------------------------------------------------------------
// Known-hard neckline projection cases (reuse of cross.rs)
// ---------------------------------------------------------------------------

/// (a) Sloped (trendline) neckline: the level must be interpolated at the
/// candle's bar-index, not read as a flat price. A descending neckline drops
/// from 1.1100 to 1.1000 over the window; the cross bar sits mid-slope, so the
/// interpolated level (~1.1050) is what the close must break.
#[test]
fn sloped_neckline_interpolated_at_bar_index() {
    // 5-bar window; neckline anchored bar0=1.1100 → bar4=1.1000 (−0.0025/bar).
    let a = "2026-06-01T10:00:00Z";
    let b = "2026-06-01T14:00:00Z";
    let ln = Line {
        name: "neckline".into(),
        a: LinePoint {
            at_epoch: ts(a).timestamp(),
            price: 1.1100,
        },
        b: LinePoint {
            at_epoch: ts(b).timestamp(),
            price: 1.1000,
        },
    };
    // Descending neckline, short → cross DOWN through the sloped line.
    let rule = bc_rule(BarEvent::OnClose, CrossDir::Down);
    let p = plan(vec![ln], vec![rule]);

    // bar2 (12:00) level = 1.1050. Seed bar1 (11:00, level 1.1075) closes above;
    // bar2 closes at 1.1040, below the interpolated 1.1050 → down close-through.
    let window = [
        candle("2026-06-01T10:00:00Z", 1.1100, 1.1105, 1.1095, 1.1100),
        candle("2026-06-01T11:00:00Z", 1.1090, 1.1095, 1.1080, 1.1085),
        candle("2026-06-01T12:00:00Z", 1.1080, 1.1085, 1.1035, 1.1040),
        candle("2026-06-01T13:00:00Z", 1.1040, 1.1045, 1.1030, 1.1038),
        candle("2026-06-01T14:00:00Z", 1.1038, 1.1042, 1.1000, 1.1010),
    ];
    // Drive bars 1..=2 one bar at a time (caller-owns-loop). For bar i the
    // window is `new_candles[..=i]`; the sloped anchors that fall outside the
    // growing prefix extrapolate in bar-index space (hourly `bar_seconds`) to
    // the same interpolated level as the full window — bar2's level is 1.1050.
    let new_candles = &window[1..=2];

    let mut facts = Facts::new();
    let effects = drive_series(&p, &mut facts, new_candles, ts("2026-06-01T12:00:05Z"));

    assert_eq!(
        fires(&effects),
        1,
        "close broke the interpolated sloped level"
    );
    assert_eq!(
        facts.at_named("neckline", "break_close"),
        Some(ts("2026-06-01T12:00:00Z"))
    );

    // Sanity: had the neckline been read as a flat 1.1000, bar2's close of
    // 1.1040 would NOT have crossed — so this genuinely exercises interpolation.
}

/// (b) Weekend / gap window: bar-index (ordinal) interpolation, not wall-clock.
/// The window has a Fri→Mon gap; the neckline advances one step per *traded bar*
/// regardless of the ~48h wall-clock gap. If interpolation used wall-clock, the
/// Monday bar's level would be badly wrong.
#[test]
fn weekend_gap_uses_bar_index_not_wallclock() {
    // Three Friday bars then three Monday bars — a real weekend gap. Neckline
    // anchored on the first Friday bar and the last Monday bar; descending.
    let window = [
        candle("2026-06-05T12:00:00Z", 1.1100, 1.1105, 1.1095, 1.1100), // Fri bar0
        candle("2026-06-05T13:00:00Z", 1.1095, 1.1100, 1.1090, 1.1095), // Fri bar1
        candle("2026-06-05T14:00:00Z", 1.1090, 1.1095, 1.1085, 1.1090), // Fri bar2
        candle("2026-06-08T09:00:00Z", 1.1085, 1.1090, 1.1080, 1.1085), // Mon bar3
        candle("2026-06-08T10:00:00Z", 1.1080, 1.1085, 1.1030, 1.1040), // Mon bar4 (cross)
        candle("2026-06-08T11:00:00Z", 1.1040, 1.1045, 1.1035, 1.1038), // Mon bar5
    ];
    // Anchors: bar0 @ 1.1100, bar5 @ 1.1050 → −0.001/bar in bar-index space.
    // bar4 level = 1.1100 − 4*0.001 = 1.1060.
    let ln = Line {
        name: "neckline".into(),
        a: LinePoint {
            at_epoch: window[0].time.timestamp(),
            price: 1.1100,
        },
        b: LinePoint {
            at_epoch: window[5].time.timestamp(),
            price: 1.1050,
        },
    };
    let rule = bc_rule(BarEvent::OnClose, CrossDir::Down);
    let p = plan(vec![ln], vec![rule]);

    // Seed bar3 closes 1.1085 above; bar4 closes 1.1040 below the interpolated
    // level → down close-through. Drive only the two Monday bars one at a time
    // (the Friday bars are window-only history for anchoring, never processed):
    // for bar i the window is `new_candles[..=i]`, and the anchors outside that
    // prefix extrapolate in bar-index space (hourly `bar_seconds`) across the
    // weekend gap — the cross still lands on bar4 (Mon 10:00).
    let new_candles = &window[3..=4];

    let mut facts = Facts::new();
    let effects = drive_series(&p, &mut facts, new_candles, ts("2026-06-08T10:00:05Z"));

    assert_eq!(
        fires(&effects),
        1,
        "cross detected against the bar-index-interpolated level across the gap"
    );
    assert_eq!(
        facts.at_named("neckline", "break_close"),
        Some(ts("2026-06-08T10:00:00Z"))
    );
}

/// (c) A cross past the second anchor's bar-index still fires — a line always
/// projects forward (the `extend_forward` field was removed; a neckline always
/// extends). The cross candle sits beyond the second anchor and must still see
/// the forward-projected level.
#[test]
fn cross_past_second_anchor_fires_via_forward_projection() {
    // Window; neckline anchored bar0 @ 1.1100 → bar2 @ 1.1000 (slope −0.005/bar).
    // Forward projection: bar3 level = 1.0950, bar4 level = 1.0900. The cross
    // candle is bar4, PAST the second anchor (bar2).
    let window = [
        candle("2026-06-01T10:00:00Z", 1.1100, 1.1105, 1.1095, 1.1100),
        candle("2026-06-01T11:00:00Z", 1.1080, 1.1085, 1.1070, 1.1075),
        candle("2026-06-01T12:00:00Z", 1.1010, 1.1015, 1.1000, 1.1005),
        // seed bar3 (past anchor): closes 1.0960, ABOVE the forward level 1.0950.
        candle("2026-06-01T13:00:00Z", 1.0980, 1.0985, 1.0958, 1.0960),
        // cross bar4 (past anchor): closes 1.0880, BELOW the forward level 1.0900.
        candle("2026-06-01T14:00:00Z", 1.0958, 1.0962, 1.0870, 1.0880),
    ];
    let anchors = (
        LinePoint {
            at_epoch: window[0].time.timestamp(),
            price: 1.1100,
        },
        LinePoint {
            at_epoch: window[2].time.timestamp(),
            price: 1.1000,
        },
    );

    // The line projects forward; bar4 crosses → fires. Drive only the two bars
    // past the anchor (bar3 seed, bar4 cross) one at a time.
    let ln = Line {
        name: "neckline".into(),
        a: anchors.0,
        b: anchors.1,
    };
    let p = plan(vec![ln], vec![bc_rule(BarEvent::OnClose, CrossDir::Down)]);
    let mut facts = Facts::new();
    let effects = drive_series(&p, &mut facts, &window[3..=4], ts("2026-06-01T14:00:05Z"));
    assert_eq!(
        fires(&effects),
        1,
        "the line projects forward past the anchor → the cross past it fires"
    );
}

// ---------------------------------------------------------------------------
// Fact model spot-check
// ---------------------------------------------------------------------------

/// The `last_close` bookkeeping is recorded (as rule-private scratch) even on a
/// non-firing bar, so a genuine cross on the *next* bar measures against the
/// right prior close. It lives in the scratch namespace keyed by the rule id —
/// NOT the shared `(line, kind)` fact map.
#[test]
fn last_close_scratch_recorded_on_seed_bar() {
    let ln = horizontal_line(
        "neckline",
        1.1000,
        "2026-06-01T09:00:00Z",
        "2026-06-01T20:00:00Z",
    );
    let rule = bc_rule(BarEvent::OnClose, CrossDir::Down);
    let p = plan(vec![ln], vec![rule]);

    // Only the seed bar — no cross yet.
    let candles = vec![candle(
        "2026-06-01T12:00:00Z",
        1.1012,
        1.1015,
        1.1008,
        1.1010,
    )];

    let mut facts = Facts::new();
    let _ = drive_series(&p, &mut facts, &candles, ts("2026-06-01T12:00:05Z"));

    assert_eq!(
        facts.get_scratch_named("03-prep-break-and-close", "last_close"),
        Some(&FactValue::Num(1.1010)),
        "seed bar's close persisted as rule-private last_close scratch"
    );
    assert!(!facts.is_set_named("neckline", "break_close"));
}

/// Reading the SHARED fact namespace must never surface the `last_close`
/// scratch — it is reachable only via the scratch accessor, keyed by rule id.
/// Guards the namespace split (a future rule reading `("neckline","last_close")`
/// must not pick up another rule's private bookkeeping).
#[test]
fn last_close_scratch_not_visible_in_shared_facts() {
    let ln = horizontal_line(
        "neckline",
        1.1000,
        "2026-06-01T09:00:00Z",
        "2026-06-01T20:00:00Z",
    );
    let rule = bc_rule(BarEvent::OnClose, CrossDir::Down);
    let p = plan(vec![ln], vec![rule]);

    let candles = vec![candle(
        "2026-06-01T12:00:00Z",
        1.1012,
        1.1015,
        1.1008,
        1.1010,
    )];

    let mut facts = Facts::new();
    let _ = drive_series(&p, &mut facts, &candles, ts("2026-06-01T12:00:05Z"));

    // Scratch is set...
    assert_eq!(
        facts.num_scratch_named("03-prep-break-and-close", "last_close"),
        Some(1.1010),
    );
    // ...but the SHARED namespace never surfaces it — under the line name it
    // used to (mistakenly) share, nor under the rule id.
    assert_eq!(
        facts.get_named("neckline", "last_close"),
        None,
        "scratch must not leak into the shared (line, kind) fact map"
    );
    assert!(!facts.is_set_named("neckline", "last_close"));
    assert_eq!(
        facts.get_named("03-prep-break-and-close", "last_close"),
        None
    );
}

// ---------------------------------------------------------------------------
// (4b) Wire round-trip — the string-on-the-wire boundary
// ---------------------------------------------------------------------------

/// The 4b boundary guard: typed geometry names serialize as their stable
/// strings, and a plan survives a serialize → deserialize round-trip AND still
/// drives identically.
///
/// This exercises the two wire facts 4b rests on:
///
/// 1. **`PlanRule` no longer carries a `line` field** — the serialized JSON must
///    contain no `"line":` key on the rule (the line is fixed by the rule's
///    type, `Neckline`, bound by the driver from `RuleKind::BreakAndClose`).
/// 2. **`Line.name` is still the wire label** — the geometry the driver finds
///    via `line_typed::<Neckline>()` resolves off the serialized `"neckline"`
///    string, so the deserialized plan stamps `("neckline","break_close")`
///    exactly as the in-memory one did.
#[test]
fn plan_wire_roundtrip_still_drives_and_stamps() {
    let ln = horizontal_line(
        "neckline",
        1.1000,
        "2026-06-01T09:00:00Z",
        "2026-06-01T20:00:00Z",
    );
    let rule = bc_rule(BarEvent::OnClose, CrossDir::Down);
    let p = plan(vec![ln], vec![rule]);

    // Serialize to the wire form and assert the removed field is gone.
    let wire = serde_json::to_string(&p).expect("plan serializes");
    assert!(
        !wire.contains("\"line\":"),
        "PlanRule must not serialize a `line` field (the line is its type): {wire}"
    );
    assert!(
        wire.contains("\"neckline\""),
        "Line.name is still the wire label the driver resolves geometry from: {wire}"
    );

    // Round-trip back and drive the DESERIALIZED plan.
    let p2: TradePlan = serde_json::from_str(&wire).expect("plan deserializes");

    let candles = vec![
        candle("2026-06-01T12:00:00Z", 1.1012, 1.1015, 1.1008, 1.1010),
        candle("2026-06-01T13:00:00Z", 1.1008, 1.1009, 1.0985, 1.0990),
    ];
    let mut facts = Facts::new();
    let effects = drive_series(&p2, &mut facts, &candles, ts("2026-06-01T13:00:05Z"));

    // Identical outcome to the in-memory plan: one fire, break_close stamped to
    // the cross candle — the `line_typed::<Neckline>()` lookup found the geometry
    // off the round-tripped `"neckline"` string.
    assert_eq!(fires(&effects), 1, "deserialized plan fires once");
    assert_eq!(
        facts.at_named("neckline", "break_close"),
        Some(ts("2026-06-01T13:00:00Z")),
        "deserialized plan stamps break_close identically"
    );
}
