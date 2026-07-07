//! Bug #15 reproduction — the GBP/USD inverse-H&S `too-low` veto that
//! (per the analysis-data report) fired on the reversals twin but not the
//! experimental twin, despite byte-identical triggers and the same TradeNation
//! mid feed.
//!
//! This test feeds the *real* TradeNation GBP/USD H1 mid candles (pulled via
//! the tradenation MCP) through `evaluate_plan` to find out, exactly, which rule
//! the engine fires and on which bar — grounding the bug instead of hand-tracing
//! `level_crossed`.
//!
//! ## Conclusion (verified by these tests)
//!
//! The pure FSM is **correct**: given the real feed, `evaluate_plan` fires
//! `01-veto-too-low` on the 15:00 UTC bar (close 1.31897 == level, `c <= level`
//! holds) and the plan goes Done — in *every* batching scenario, including a
//! single coalesced catch-up batch that also contains the 10:00 expiry bar
//! (so rule-ordering is not the cause either).
//!
//! Therefore bug #15's premise — "the engine's intrabar level veto silently
//! failed" — is wrong. The divergence between the twins is **not** in
//! `evaluate_plan`. The remaining live mechanism is at the wrapper level: a
//! plan-state **re-seed** (`tick_one` → `seed_first_tick` → `seed_plan_state`
//! when `get_plan_state` returns `None`) advances the watermark to the newest
//! candle and **fires nothing**, so any cross already in the un-processed gap is
//! skipped forever (the next fetch only asks for newer-than-watermark). The
//! wall-clock `trade-expiry` survives a re-seed (it re-becomes true at any tick
//! at or after its epoch); a price-cross veto does not. That matches the experimental
//! twin exactly: watermark jumped past the cross, only trade-expiry fired. A KV
//! TTL lapse / read-miss / non-landing `put` on one account's state row — but
//! not the other's, registered 2 min apart — is enough to diverge the twins.

use chrono::{DateTime, Utc};

use trade_control_core::broker::{Candle, Granularity};
use trade_control_core::intent::{Action, BrokerKind, Direction, Intent, VetoLevel};
use trade_control_core::plan_state::Phase;
use trade_control_core::trade_plan::{
    BarEvent, ConditionRule, CrossDir, FireMode, TradePlan, Trigger,
};
use trade_control_core::tunable::Tunable;
use trade_control_engine::{evaluate_plan, seed_plan_state};

fn ts(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn c(time: &str, o: f64, h: f64, l: f64, cl: f64) -> Candle {
    Candle {
        time: ts(time),
        o,
        h,
        l,
        c: cl,
    }
}

/// The real TradeNation GBP/USD H1 mid candles spanning register → expiry,
/// ascending. Timestamps are bar-open in UTC (TradeNation labels bars by open).
fn gbp_usd_mid() -> Vec<Candle> {
    vec![
        c("2026-06-23T13:00:00Z", 1.32118, 1.32222, 1.32049, 1.32135),
        c("2026-06-23T14:00:00Z", 1.32135, 1.32154, 1.31950, 1.32010),
        c("2026-06-23T15:00:00Z", 1.32009, 1.32024, 1.31850, 1.31897), // intrabar clip, close == level
        c("2026-06-23T16:00:00Z", 1.31896, 1.31972, 1.31859, 1.31867), // straddles AND close below
        c("2026-06-23T17:00:00Z", 1.31868, 1.31915, 1.31830, 1.31875),
        c("2026-06-23T18:00:00Z", 1.31875, 1.31890, 1.31824, 1.31889),
        c("2026-06-23T19:00:00Z", 1.31888, 1.32006, 1.31880, 1.31953),
        c("2026-06-23T20:00:00Z", 1.31954, 1.32041, 1.31947, 1.32037),
        c("2026-06-23T21:00:00Z", 1.32004, 1.32055, 1.32003, 1.32019),
        c("2026-06-23T22:00:00Z", 1.32019, 1.32020, 1.31984, 1.32006),
        c("2026-06-23T23:00:00Z", 1.32007, 1.32038, 1.31972, 1.31996),
        c("2026-06-24T00:00:00Z", 1.31997, 1.32031, 1.31939, 1.31991),
        c("2026-06-24T01:00:00Z", 1.31992, 1.32020, 1.31909, 1.32012),
        c("2026-06-24T02:00:00Z", 1.32011, 1.32048, 1.31905, 1.31908),
        c("2026-06-24T03:00:00Z", 1.31908, 1.32017, 1.31872, 1.31989),
        c("2026-06-24T04:00:00Z", 1.31989, 1.31991, 1.31908, 1.31930),
        c("2026-06-24T05:00:00Z", 1.31931, 1.31982, 1.31924, 1.31952),
        c("2026-06-24T06:00:00Z", 1.31951, 1.32094, 1.31840, 1.31867),
        c("2026-06-24T07:00:00Z", 1.31866, 1.31989, 1.31800, 1.31869),
        c("2026-06-24T08:00:00Z", 1.31869, 1.31978, 1.31714, 1.31927),
        c("2026-06-24T09:00:00Z", 1.31927, 1.31958, 1.31747, 1.31755),
        c("2026-06-24T10:00:00Z", 1.31756, 1.31760, 1.31549, 1.31558),
    ]
}

fn veto_intent(id: &str, name: &str, level: VetoLevel) -> Intent {
    let mut i = base_intent(Action::Veto, id);
    i.name = Some(name.into());
    i.level = Some(level);
    i
}

fn base_intent(action: Action, id: &str) -> Intent {
    Intent {
        entry_level_vetos: Vec::new(),
        v: 1,
        id: id.into(),
        not_before: None,
        not_after: ts("2026-06-24T10:30:00Z"),
        action,
        instrument: "GBP/USD".into(),
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
        broker: BrokerKind::TradeNation,
        account: Some("experimental".into()),
        step: None,
        name: None,
        ttl_hours: Tunable::Static(31),
        level: None,
        requires_preps: Vec::new(),
        vetos: Vec::new(),
        clears: Vec::new(),
        trade_id: Some("ihs-gbp-usd-5175abbe".into()),
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
        pip_size: Some(0.0001),
        spread_window: None,
        trade_plan: None,
        blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
        breakeven: None,
        include_archived: false,
    }
}

/// The experimental twin's rules, verbatim from `plan show ihs-gbp-usd-5175abbe`
/// — minus the `05-enter` Pine rule (it never fired; `not_after` 03:56 and the
/// detector needs a wide window). The veto behaviour is independent of the enter.
fn experimental_plan() -> TradePlan {
    let too_low = ConditionRule {
        rule_id: "01-veto-too-low".into(),
        trigger: Trigger::HorizontalCross {
            level: 1.31897,
            dir: CrossDir::Down,
            bar: BarEvent::Intrabar,
        },
        fire_mode: FireMode::Once,
        intent: veto_intent(
            "ihs-gbp-usd-5175abbe-too-low",
            "too-low",
            VetoLevel::ClosePositions,
        ),
    };
    let too_high = ConditionRule {
        rule_id: "01-veto-too-high".into(),
        trigger: Trigger::PriceValueCross {
            level: 1.329607884595391,
            dir: CrossDir::Either,
            bar: BarEvent::Intrabar,
        },
        fire_mode: FireMode::Once,
        intent: veto_intent(
            "ihs-gbp-usd-5175abbe-too-high",
            "too-high",
            VetoLevel::StopNextEntry,
        ),
    };
    let mut expiry_intent = veto_intent(
        "ihs-gbp-usd-5175abbe-trade-expiry",
        "trade-expiry",
        VetoLevel::ClosePositions,
    );
    expiry_intent.not_before = Some(ts("2026-06-24T10:00:00Z"));
    let trade_expiry = ConditionRule {
        rule_id: "02-veto-trade-expiry".into(),
        trigger: Trigger::TimeReached {
            at_epoch: 1782295200,
        },
        fire_mode: FireMode::Once,
        intent: expiry_intent,
    };
    TradePlan {
        trade_id: "ihs-gbp-usd-5175abbe".into(),
        instrument: "GBP/USD".into(),
        direction: Direction::Long,
        granularity: Granularity::H1,
        pip_size: 0.0001,
        rules: vec![too_low, too_high, trade_expiry],
        shadow: false,
        cross_buffer_pct: 0.0,
        retest_atr_step: trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
        replay_start: None,
    }
}

#[test]
fn bug015_too_low_fires_on_the_cross_not_expiry() {
    let plan = experimental_plan();
    let candles = gbp_usd_mid();
    let expires = ts("2026-06-25T10:00:00Z");

    // Seed without firing on the first ~2 bars (register was 13:44 UTC, so the
    // seed back-window's newest bar is the 13:00 bar). Then tick forward over
    // everything from 14:00 onward in one catch-up batch (the cron may coalesce).
    let seed = &candles[..1]; // 13:00 bar only
    let state = seed_plan_state(&plan, seed, expires);
    assert_eq!(
        state.phase,
        Phase::AwaitEntry,
        "no break-and-close → AwaitEntry"
    );
    assert_eq!(state.watermark, Some(ts("2026-06-23T13:00:00Z")));

    let fresh = &candles[1..]; // 14:00 .. 10:00 next day
    let eval = evaluate_plan(
        &plan,
        &state,
        fresh,
        fresh,
        ts("2026-06-24T10:05:00Z"),
        expires,
        &trade_control_core::position_view::NoPositions,
    );

    let fired_ids: Vec<&str> = eval.fired.iter().map(|f| f.rule_id.as_str()).collect();
    let fire_bar = eval.fired.first().map(|f| f.candle.time);
    eprintln!(
        "fired = {fired_ids:?} on bar {fire_bar:?}; final phase {:?} watermark {:?}",
        eval.new_state.phase, eval.new_state.watermark
    );

    assert_eq!(
        fired_ids,
        vec!["01-veto-too-low"],
        "too-low must fire (and before trade-expiry); got {fired_ids:?}"
    );
    // Fires on the 15:00 bar: its close (1.31897) equals the level exactly and
    // `c <= level` holds, so the intrabar-Down guard fires immediately — one bar
    // earlier than the "unambiguous close-below" 16:00 bar.
    assert_eq!(
        fire_bar,
        Some(ts("2026-06-23T15:00:00Z")),
        "too-low fires on the first straddling bar whose close sits at/below the level"
    );
    assert!(
        eval.done,
        "a ClosePositions veto is terminal — plan goes Done"
    );
}

/// Hypothesis (B) from the report: if a single coalesced catch-up batch contains
/// BOTH the too-low cross bars and the 10:00 expiry bar, does rule-ordering fire
/// trade-expiry first? It must not: the per-candle loop processes bars in time
/// order, and the 15:00 cross bar is reached (and ends the plan) long before the
/// 10:00 expiry bar. This proves ordering is not the bug.
#[test]
fn bug015_too_low_wins_even_in_one_batch_with_expiry() {
    let plan = experimental_plan();
    let candles = gbp_usd_mid();
    let expires = ts("2026-06-25T10:00:00Z");

    // Seed at the 14:00 bar, then feed EVERYTHING (15:00 .. 10:00 incl. expiry)
    // as one batch — the worst case for ordering.
    let state = seed_plan_state(&plan, &candles[..2], expires);
    assert_eq!(state.watermark, Some(ts("2026-06-23T14:00:00Z")));

    let fresh = &candles[2..];
    let eval = evaluate_plan(
        &plan,
        &state,
        fresh,
        fresh,
        ts("2026-06-24T10:05:00Z"),
        expires,
        &trade_control_core::position_view::NoPositions,
    );
    let fired_ids: Vec<&str> = eval.fired.iter().map(|f| f.rule_id.as_str()).collect();

    assert_eq!(fired_ids, vec!["01-veto-too-low"]);
    assert_eq!(eval.fired[0].candle.time, ts("2026-06-23T15:00:00Z"));
}

/// The **actual bug mechanism**: a plan-state re-seed after the cross already
/// passed. If `get_plan_state` returns `None` mid-life (KV TTL lapse, an
/// eventually-consistent read miss, or a `put` that didn't land), `tick_one`
/// re-seeds via `seed_plan_state`, which sets the watermark to the newest candle
/// and **fires nothing**. Any cross in the skipped gap is then lost forever (the
/// next fetch only asks for `> watermark`).
///
/// Here we re-seed at the 09:00 bar — *after* the 15:00–17:00 too-low cross — and
/// then tick the remaining bars. too-low never fires; only the wall-clock
/// trade-expiry (which re-becomes true at any tick >= its epoch, immune to a
/// re-seed) does. This reproduces the experimental twin's exact terminal state:
/// `fired = [02-veto-trade-expiry]`, watermark 10:00.
#[test]
fn bug015_reseed_after_cross_skips_too_low_and_only_expiry_fires() {
    let plan = experimental_plan();
    let candles = gbp_usd_mid();
    let expires = ts("2026-06-25T10:00:00Z");

    // Re-seed using a back-window whose newest bar is 09:00 (index 20) — the
    // cross at 15:00–17:00 the prior day is now behind the watermark.
    let reseed_window = &candles[..21]; // 13:00 .. 09:00, newest = 09:00
    let state = seed_plan_state(&plan, reseed_window, expires);
    assert_eq!(
        state.watermark,
        Some(ts("2026-06-24T09:00:00Z")),
        "re-seed jumps the watermark past the cross"
    );
    assert!(
        state.fired.is_empty(),
        "re-seed fires nothing — the cross is silently skipped"
    );

    // Tick the only remaining bar (10:00, the expiry bar).
    let fresh = &candles[21..]; // just the 10:00 bar
    let eval = evaluate_plan(
        &plan,
        &state,
        fresh,
        fresh,
        ts("2026-06-24T10:05:00Z"),
        expires,
        &trade_control_core::position_view::NoPositions,
    );
    let fired_ids: Vec<&str> = eval.fired.iter().map(|f| f.rule_id.as_str()).collect();

    assert_eq!(
        fired_ids,
        vec!["02-veto-trade-expiry"],
        "after a re-seed past the cross, only the wall-clock expiry fires — \
         exactly the experimental twin's terminal state"
    );
    assert_eq!(eval.new_state.watermark, Some(ts("2026-06-24T10:00:00Z")));
}
