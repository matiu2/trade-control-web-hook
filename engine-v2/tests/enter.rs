//! Fact-based enter tests for engine-v2.
//!
//! The enter is the first rule to emit an **acquisitive** effect
//! ([`Effect::PlaceOrder`]). It reads its own [`PlanRule::preps`] map — the
//! layered `line -> [ordered milestone kinds]` precondition model — and places
//! only when every line's chain is satisfied (all milestones present AND strictly
//! ordered within each line, lines independent).
//!
//! Most tests **pre-seed the `Facts` blackboard** with milestone facts and tick
//! the enter alone — isolating the satisfaction check from break/retest cross
//! mechanics. The last test drives the full break → retest → enter chain through
//! the driver. One test exercises the driver's live-bar catch-up gate (a
//! `PlaceOrder` on a backlog bar is dropped; the facts still apply).

use chrono::{DateTime, Utc};

use trade_control_core::broker::{Candle, Granularity};
use trade_control_core::intent::{Action, BrokerKind, Direction, Intent};
use trade_control_core::trade_plan::{BarEvent, CrossDir, DEFAULT_RETEST_ATR_STEP, LinePoint};
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

/// A minimal enter intent — only `action`/`instrument` matter for the effect
/// assertions; the rest is copied verbatim into the fired result.
fn enter_intent(instrument: &str) -> Intent {
    Intent {
        entry_level_vetos: Vec::new(),
        v: 1,
        id: "05-enter".into(),
        not_before: None,
        not_after: ts("2026-06-30T00:00:00Z"),
        action: Action::Enter,
        instrument: instrument.into(),
        direction: Some(Direction::Long),
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

/// An enter rule with the given precondition map and mechanism.
fn enter_rule(instrument: &str, preps: PrepMap, mechanism: EntryMechanism) -> PlanRule {
    PlanRule {
        id: "05-enter".into(),
        // The enter references lines only through its `preps` map keys (runtime
        // names), not a fixed geometry line — so there is no `line` field to set.
        kind: RuleKind::Enter,
        intent: enter_intent(instrument),
        bar: BarEvent::OnClose,
        dir: CrossDir::Up,
        preps,
        mechanism,
    }
}

/// A one-line prep map: `{ line: kinds }`.
fn prep_map(line: &str, kinds: &[&str]) -> PrepMap {
    let mut m = PrepMap::new();
    m.insert(line.into(), kinds.iter().map(|k| k.to_string()).collect());
    m
}

/// A plan holding just the enter rule (facts are pre-seeded in most tests).
fn enter_only_plan(instrument: &str, enter: PlanRule) -> TradePlan {
    TradePlan {
        trade_id: "t-enter".into(),
        instrument: instrument.into(),
        direction: Direction::Long,
        granularity: Granularity::H1,
        lines: vec![Line {
            name: "neckline".into(),
            a: LinePoint {
                at_epoch: ts("2026-06-01T00:00:00Z").timestamp(),
                price: 1.0,
            },
            b: LinePoint {
                at_epoch: ts("2026-06-01T10:00:00Z").timestamp(),
                price: 1.0,
            },
        }],
        levels: Vec::new(),
        markers: Vec::new(),
        pause_windows: Vec::new(),
        rules: vec![enter],
        cross_buffer_pct: 0.0,
        retest_atr_step: DEFAULT_RETEST_ATR_STEP,
    }
}

/// Count the `PlaceOrder` effects in a fire list.
fn place_orders(fires: &[Effect]) -> Vec<&Effect> {
    fires
        .iter()
        .filter(|e| matches!(e, Effect::PlaceOrder { .. }))
        .collect()
}

// A dummy candle we tick the enter against; its geometry is irrelevant to the
// pre-seeded-facts tests (the enter reads facts, not this bar's OHLC).
fn a_bar() -> Candle {
    candle("2026-06-02T05:00:00Z", 1.10, 1.11, 1.09, 1.10)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// With only `break_close` stamped (retest missing), the enter does NOT place.
#[test]
fn enter_blocked_until_all_preps_satisfied() {
    let plan = enter_only_plan(
        "EUR_USD",
        enter_rule(
            "EUR_USD",
            prep_map("neckline", &["break_close", "retest"]),
            EntryMechanism::Stop,
        ),
    );
    let mut facts = Facts::new();
    // Only the first milestone is present.
    facts.set_named(
        "neckline",
        "break_close",
        FactValue::At(ts("2026-06-02T03:00:00Z")),
    );

    let bar = a_bar();
    let fires = tick_once(&plan, &mut facts, &[bar], bar.time, true);

    assert!(
        place_orders(&fires).is_empty(),
        "enter must not place until every milestone is stamped",
    );
    // And no terminal entry outcome was stamped (the driver stamps that on a
    // resolved placement; nothing placed here).
    assert!(!facts.is_set_named("05-enter", "entry_outcome"));
}

/// The chain is out of order (retest stamped BEFORE break_close) — the enter
/// declares the order and rejects it, even though both facts are present.
#[test]
fn enter_requires_monotonic_prep_order() {
    let plan = enter_only_plan(
        "EUR_USD",
        enter_rule(
            "EUR_USD",
            prep_map("neckline", &["break_close", "retest"]),
            EntryMechanism::Stop,
        ),
    );
    let mut facts = Facts::new();
    // retest stamped EARLIER than break_close — pathological ordering.
    facts.set_named(
        "neckline",
        "break_close",
        FactValue::At(ts("2026-06-02T04:00:00Z")),
    );
    facts.set_named(
        "neckline",
        "retest",
        FactValue::At(ts("2026-06-02T03:00:00Z")),
    );

    let bar = a_bar();
    let fires = tick_once(&plan, &mut facts, &[bar], bar.time, true);

    assert!(
        place_orders(&fires).is_empty(),
        "enter must reject a chain whose milestones are not strictly increasing in list order",
    );
}

/// Both milestones present and in order → the enter emits exactly one
/// `PlaceOrder` carrying the enter intent + mechanism.
///
/// Single-shot behaviour (not double-placing on the next bar) is driven by the
/// driver-stamped `entry_outcome` fact, which the driver does NOT yet write in
/// this slice — so re-ticking here would re-emit. That fire-once guard is pinned
/// by `enter_done_once_entry_outcome_stamped` below (simulating the driver's
/// stamp) and wired for real in the next slice (driver late-entry routing).
#[test]
fn enter_fires_when_chain_complete() {
    let plan = enter_only_plan(
        "EUR_USD",
        enter_rule(
            "EUR_USD",
            prep_map("neckline", &["break_close", "retest"]),
            EntryMechanism::Stop,
        ),
    );
    let mut facts = Facts::new();
    facts.set_named(
        "neckline",
        "break_close",
        FactValue::At(ts("2026-06-02T03:00:00Z")),
    );
    facts.set_named(
        "neckline",
        "retest",
        FactValue::At(ts("2026-06-02T04:00:00Z")),
    );

    let bar = a_bar();
    let fires = tick_once(&plan, &mut facts, &[bar], bar.time, true);

    let orders = place_orders(&fires);
    assert_eq!(orders.len(), 1, "one PlaceOrder when the chain is complete");
    match orders[0] {
        Effect::PlaceOrder {
            fired,
            mechanism,
            trigger_price,
            candle_close,
        } => {
            assert_eq!(fired.rule_id, "05-enter");
            assert_eq!(fired.intent.action, Action::Enter);
            assert_eq!(fired.intent.instrument, "EUR_USD");
            assert_eq!(*mechanism, EntryMechanism::Stop);
            // Trigger resolution is the executor's job (later slice) → None today.
            assert_eq!(*trigger_price, None);
            // candle_close is the close of the firing bar (a_bar closes at 1.10).
            assert_eq!(*candle_close, bar.c);
        }
        _ => unreachable!(),
    }
}

/// The single-shot fire-once guard: the enter reads a `(rule_id, "entry_outcome")`
/// fact — which the DRIVER stamps when it resolves a placement — and stops
/// emitting once it is set (the enter is done). Here we simulate the driver's
/// stamp by setting the fact directly, then assert the enter goes quiet. (The
/// driver actually stamps it in the next slice's late-entry routing; this pins the
/// rule's half of the contract now.)
#[test]
fn enter_done_once_entry_outcome_stamped() {
    let plan = enter_only_plan(
        "EUR_USD",
        enter_rule(
            "EUR_USD",
            prep_map("neckline", &["break_close", "retest"]),
            EntryMechanism::Stop,
        ),
    );
    let mut facts = Facts::new();
    facts.set_named(
        "neckline",
        "break_close",
        FactValue::At(ts("2026-06-02T03:00:00Z")),
    );
    facts.set_named(
        "neckline",
        "retest",
        FactValue::At(ts("2026-06-02T04:00:00Z")),
    );
    // Simulate the driver having resolved a placement → terminal entry outcome.
    facts.set_named(
        "05-enter",
        "entry_outcome",
        FactValue::At(ts("2026-06-02T05:00:00Z")),
    );

    let bar = a_bar();
    let fires = tick_once(&plan, &mut facts, &[bar], bar.time, true);
    assert!(
        place_orders(&fires).is_empty(),
        "enter must stay quiet once a terminal entry_outcome is stamped (single-shot)",
    );
}

/// Two prep lines, both required. The enter waits until BOTH lines' chains
/// complete — independent of each other, no cross-line ordering.
#[test]
fn enter_independent_across_lines() {
    let mut preps = prep_map("neckline", &["break_close", "retest"]);
    preps.insert("supportline".into(), vec!["broke_below".into()]);

    let plan = enter_only_plan(
        "EUR_USD",
        enter_rule("EUR_USD", preps, EntryMechanism::Stop),
    );
    let mut facts = Facts::new();

    // neckline chain complete; supportline NOT yet → no place.
    facts.set_named(
        "neckline",
        "break_close",
        FactValue::At(ts("2026-06-02T03:00:00Z")),
    );
    facts.set_named(
        "neckline",
        "retest",
        FactValue::At(ts("2026-06-02T04:00:00Z")),
    );
    let bar = a_bar();
    let fires = tick_once(&plan, &mut facts, &[bar], bar.time, true);
    assert!(
        place_orders(&fires).is_empty(),
        "all lines must be satisfied, not just one",
    );

    // Now satisfy supportline (a time UNRELATED to the neckline chain — lines are
    // independent, so its ordering vs the neckline doesn't matter).
    facts.set_named(
        "supportline",
        "broke_below",
        FactValue::At(ts("2026-06-02T02:00:00Z")),
    );
    let bar2 = candle("2026-06-02T06:00:00Z", 1.10, 1.11, 1.09, 1.10);
    let fires2 = tick_once(&plan, &mut facts, &[bar2], bar2.time, true);
    assert_eq!(
        place_orders(&fires2).len(),
        1,
        "enter places once BOTH independent lines are satisfied",
    );
}

/// An empty prep map is a no-prep enter — vacuously satisfied, places on the
/// first (live) bar.
#[test]
fn no_prep_enter_places_immediately() {
    let plan = enter_only_plan(
        "EUR_USD",
        enter_rule("EUR_USD", PrepMap::new(), EntryMechanism::Stop),
    );
    let mut facts = Facts::new();

    let bar = a_bar();
    let fires = tick_once(&plan, &mut facts, &[bar], bar.time, true);
    assert_eq!(
        place_orders(&fires).len(),
        1,
        "a no-prep enter places as soon as it ticks",
    );
}

/// Second fire-once guard: a no-prep enter that WOULD place immediately does
/// **not** place when the plan is retired — the plan-scoped `(__plan__,
/// "invalidated")` fact is set (as the driver stamps it on an `Effect::Invalidate`
/// from an invalidation cap). The enter is done, StopNextEntry-only.
#[test]
fn retired_plan_blocks_the_enter() {
    let plan = enter_only_plan(
        "EUR_USD",
        enter_rule("EUR_USD", PrepMap::new(), EntryMechanism::Stop),
    );
    let mut facts = Facts::new();
    // Simulate a prior invalidation-cap cross: the driver stamped the retire fact.
    facts.set_named(
        "__plan__",
        "invalidated",
        FactValue::At(ts("2026-06-01T00:00:00Z")),
    );

    let bar = a_bar();
    let fires = tick_once(&plan, &mut facts, &[bar], bar.time, true);
    assert!(
        place_orders(&fires).is_empty(),
        "a retired plan blocks the enter even with satisfied (empty) preps",
    );
}

/// Catch-up safety (interim): the chain is complete but this is a **backlog** bar
/// (`latest_bar = false`), so the driver drops the `PlaceOrder`. The enter does
/// not go quiet on its own (that is driven by a driver-stamped `entry_outcome`,
/// unset here), so when the setup is still satisfied on the following **latest**
/// bar the enter re-emits and the driver keeps it.
///
/// This pins the *interim* blunt-drop behaviour. The next slice replaces the
/// blunt drop with `late_entry::resolve` (missed-vs-place-late parity) and stamps
/// `entry_outcome` — at which point a backlog whose order would still be resting
/// place-lates on the latest bar, and a would-have-triggered one is marked missed
/// (and never re-placed). `late_entry.rs`'s own unit tests already pin that logic;
/// this test pins the driver's latest-bar gate on the acquisitive effect.
#[test]
fn enter_dropped_on_backlog_bar_then_places_on_latest() {
    let plan = enter_only_plan(
        "EUR_USD",
        enter_rule(
            "EUR_USD",
            prep_map("neckline", &["break_close", "retest"]),
            EntryMechanism::Stop,
        ),
    );
    let mut facts = Facts::new();
    facts.set_named(
        "neckline",
        "break_close",
        FactValue::At(ts("2026-06-02T03:00:00Z")),
    );
    facts.set_named(
        "neckline",
        "retest",
        FactValue::At(ts("2026-06-02T04:00:00Z")),
    );

    // Backlog bar: preconditions satisfied, but not the latest bar → PlaceOrder
    // dropped by the driver.
    let backlog = candle("2026-06-02T05:00:00Z", 1.10, 1.11, 1.09, 1.10);
    let fires = tick_once(&plan, &mut facts, &[backlog], backlog.time, false);
    assert!(
        place_orders(&fires).is_empty(),
        "an acquisitive PlaceOrder must be dropped on a stale backlog bar",
    );

    // The latest bar re-ticks: the setup is still valid (facts persisted) → it now
    // places at the current price. This is the "don't chase a stale entry, but
    // take it if it's still valid now" property.
    let latest = candle("2026-06-02T06:00:00Z", 1.10, 1.11, 1.09, 1.10);
    let fires2 = tick_once(&plan, &mut facts, &[latest], latest.time, true);
    assert_eq!(
        place_orders(&fires2).len(),
        1,
        "the latest bar places once the caught-up facts satisfy the preconditions",
    );
}

/// End-to-end: break-and-close → retest → enter, all three rules through the
/// driver over a real candle series. The enter places after both preps stamp.
#[test]
fn end_to_end_break_retest_enter() {
    // Horizontal neckline at 1.10, direction Up (a long H&S-style setup: close
    // above the neckline = break, dip back to it = retest).
    let neckline = Line {
        name: "neckline".into(),
        a: LinePoint {
            at_epoch: ts("2026-06-01T00:00:00Z").timestamp(),
            price: 1.10,
        },
        b: LinePoint {
            at_epoch: ts("2026-06-01T10:00:00Z").timestamp(),
            price: 1.10,
        },
    };

    let bc = PlanRule {
        id: "03-prep-break-and-close".into(),
        kind: RuleKind::BreakAndClose,
        intent: {
            let mut i = enter_intent("EUR_USD");
            i.action = Action::Prep;
            i.id = "03".into();
            i
        },
        bar: BarEvent::OnClose,
        dir: CrossDir::Up,
        preps: PrepMap::new(),
        mechanism: EntryMechanism::Stop,
    };
    let retest = PlanRule {
        id: "04-prep-retest".into(),
        kind: RuleKind::Retest,
        intent: {
            let mut i = enter_intent("EUR_USD");
            i.action = Action::Prep;
            i.id = "04".into();
            i
        },
        bar: BarEvent::Intrabar,
        dir: CrossDir::Down,
        preps: PrepMap::new(),
        mechanism: EntryMechanism::Stop,
    };
    let enter = enter_rule(
        "EUR_USD",
        prep_map("neckline", &["break_close", "retest"]),
        EntryMechanism::Stop,
    );

    let plan = TradePlan {
        trade_id: "t-e2e".into(),
        instrument: "EUR_USD".into(),
        direction: Direction::Long,
        granularity: Granularity::H1,
        lines: vec![neckline],
        levels: Vec::new(),
        markers: Vec::new(),
        pause_windows: Vec::new(),
        rules: vec![bc, retest, enter],
        cross_buffer_pct: 0.0,
        retest_atr_step: DEFAULT_RETEST_ATR_STEP,
    };

    // A warmup ramp under the neckline (so ATR is warm), then a break-and-close
    // above 1.10, then a dip back to 1.10 (retest), then a continuation bar.
    //
    // Times are chosen to AVOID EUR_USD's learned spread hours (21:00–05:00Z),
    // which would suppress the break/retest crosses as "rubbish candles". The
    // break/retest/continuation land at 12:00–14:00Z (mid-London session); the
    // warmup precedes them from 22:00 the day before through 11:00.
    let start = ts("2026-05-31T16:00:00Z");
    let mut candles = Vec::new();
    // 20 warmup bars climbing from 1.05 to just under 1.10. Spread-hour bars in
    // this span only carry warmup (no cross is attempted on them), so they're
    // harmless here.
    for k in 0..20 {
        let base = 1.05 + (k as f64) * 0.002;
        let t = start + chrono::Duration::hours(k);
        candles.push(Candle {
            time: t,
            o: base,
            h: base + 0.001,
            l: base - 0.001,
            c: base,
        });
    }
    // Break-and-close bar (12:00Z, non-spread): closes at 1.105, above the neckline.
    candles.push(candle("2026-06-01T12:00:00Z", 1.099, 1.106, 1.098, 1.105));
    // Retest bar (13:00Z): dips back to 1.10 (low touches the neckline), closes above.
    candles.push(candle("2026-06-01T13:00:00Z", 1.104, 1.106, 1.100, 1.104));
    // Continuation bar (14:00Z, live) — enter should place here (preps both stamped).
    candles.push(candle("2026-06-01T14:00:00Z", 1.104, 1.108, 1.103, 1.107));

    let now = candles.last().expect("non-empty").time;
    let mut facts = Facts::new();
    let mut all_fires = Vec::new();
    for i in 0..candles.len() {
        // The last bar of the series is the latest bar; the rest are historical
        // but for this in-order replay every bar is the latest as it is processed.
        // Drive them all as latest (a fresh replay, not a downtime backlog).
        all_fires.extend(tick_once(&plan, &mut facts, &candles[..=i], now, true));
    }

    // Both preps stamped, in order.
    let bc_at = facts
        .at_named("neckline", "break_close")
        .expect("break_close set");
    let retest_at = facts.at_named("neckline", "retest").expect("retest set");
    assert!(bc_at < retest_at, "break_close before retest");

    // And the enter placed after the full break → retest chain completed. It
    // emits on every bar its preconditions hold — here the retest bar and the
    // continuation bar — so ≥1 place. Exact single-shot (place ONCE) is enforced
    // by the driver-stamped `entry_outcome` fire-once guard, wired in the next
    // slice; this e2e drives the rules directly with no driver stamping, so it
    // pins that the chain PRODUCES a placement, not the dedup.
    assert!(
        !place_orders(&all_fires).is_empty(),
        "enter places after the full break → retest chain",
    );
}
