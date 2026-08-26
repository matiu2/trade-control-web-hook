//! Fold a built trade's chart roles + signed intents into ONE
//! [`TradePlan`](trade_control_core::trade_plan::TradePlan) for the
//! server-side engine.
//!
//! This is the inverse of [`crate::alert_spec`]: instead of emitting one
//! TradingView alert per condition, it walks the same
//! [`BuiltAlert`](trade_control_cli::BuiltAlert) set and the same [`Roles`]
//! geometry and produces one [`ConditionRule`] per alert — each carrying the
//! exact same [`Intent`] the TV alert would have POSTed, plus the trigger the
//! engine evaluates itself. The `(ConditionType, Frequency)` decisions are
//! ported verbatim from `alert_spec.rs` and re-expressed in the engine's
//! [`CrossDir`] / [`BarEvent`] / [`FireMode`] split (see the `trade_plan`
//! module docs for why TV's single `Frequency` becomes two fields).
//!
//! **Commit 2a scope:** this is the *pure* builder + a chart-resolution →
//! [`Granularity`] mapper, with table tests. The plan it returns is built and
//! (in the pipeline) written to disk / logged, but **not yet POSTed** — the
//! direct `register` POST to the worker is Commit 2b.
//!
//! Alerts whose supporting role isn't on the chart are skipped (the same
//! `Ok(None)` semantics `build_alert_spec` uses), so a trade missing, say, a
//! retest trendline simply yields a plan without that rule.

use trade_control_cli::{BuiltAlert, BuiltNews, BuiltPause};
use trade_control_conventions::{AlertBasename, Direction as ConvDirection, RuleKind};
use trade_control_core::broker::Granularity;
use trade_control_core::intent::{Direction, Intent};
use trade_control_core::trade_plan::{
    BarEvent, ConditionRule, CrossDir, FireMode, TradePlan, Trigger,
};

use crate::geometry::pcl_exhausted_price;
use crate::mw_geometry::{abort_level, cancel_level, highest_shoulder, overshoot_level};
use crate::plan_geometry::{Line, PlanGeometry};

/// Arm-time inputs for the pullback prep, captured by the pipeline and threaded
/// into the plan build. `anchor_open` is the live mid at arm time (baked onto the
/// `Trigger::PullbackFromArm` so the engine never rediscovers it); `atr_mult` is
/// the `--pull-back` multiple. `None` when no pullback is armed.
#[derive(Debug, Clone, Copy)]
pub struct PullbackArm {
    pub anchor_open: f64,
    pub atr_mult: f64,
}

/// Whether this is a **trend-follow** arm (`tv-arm --trend`).
///
/// A newtype rather than a bare `bool` on purpose: [`build_trade_plan`] already
/// takes several positional `bool`s (`is_mw`, `shadow`, `bcr_require_golden`),
/// and a transposed one is exactly the failure this crate keeps hitting —
/// silent, plausible, tests-green (see `plan_geometry`'s module doc on dropped
/// wires). A distinct type makes the transposition a compile error.
///
/// What it changes: the **pcl-exhausted** veto (the computed ~80%-to-TP fib
/// level) becomes close-confirmed instead of wick-triggered. See
/// [`invalidation_or_pcl_trigger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TrendFollow(pub bool);

impl TrendFollow {
    /// The default: a reversal setup, pcl-exhausted reads the wick.
    pub const REVERSAL: Self = Self(false);
    /// A trend-continuation setup, pcl-exhausted must close past the level.
    pub const TREND: Self = Self(true);

    /// Which [`BarEvent`] the pcl-exhausted fib level is read with.
    ///
    /// A trend runs further and wicks deeper than a reversal, so the default
    /// straddle aborts the setup on a spike that closes back inside (the
    /// 2026-08-07 short: `01-veto-too-low` fired off a wick). Under `--trend`
    /// the bar must **close** past the level.
    fn pcl_bar_event(self) -> BarEvent {
        if self.0 {
            BarEvent::OnClose
        } else {
            BarEvent::Intrabar
        }
    }
}

/// Map a TradingView chart-resolution string (`"1"`, `"15"`, `"60"`, `"240"`,
/// `"D"`, …) to the engine's [`Granularity`]. The engine only fetches the
/// closed set of timeframes trades arm on, so an unsupported resolution
/// (sub-minute, weekly, anything not in the set) returns `None` and the caller
/// rejects — better than silently arming a plan the engine can't poll.
pub fn resolution_to_granularity(resolution: &str) -> Option<Granularity> {
    match resolution.trim() {
        "1" => Some(Granularity::M1),
        "5" => Some(Granularity::M5),
        "15" => Some(Granularity::M15),
        "60" => Some(Granularity::H1),
        "240" => Some(Granularity::H4),
        "D" | "1D" => Some(Granularity::D1),
        _ => None,
    }
}

/// Build the engine plan for a freshly-built trade.
///
/// - `trade_id` / `instrument` come straight off the
///   [`BuiltTrade`](trade_control_cli::BuiltTrade).
/// - `alerts` are that trade's built alerts — each supplies the embedded
///   [`Intent`] and the basename the trigger is keyed on. (Taken as a slice
///   rather than the whole `BuiltTrade` so this stays decoupled from
///   `TradeSpec` and trivially testable.)
/// - `direction` is the resolved trade direction (H&S or M/W).
/// - `roles` supplies the chart geometry every price/time trigger reads.
/// - `granularity` is the chart timeframe (via [`resolution_to_granularity`]).
/// - `is_mw` switches the `05-enter` rule between the H&S pattern trigger and
///   the M/W per-bar heartbeat, mirroring `build_alert_spec`.
/// - `shadow` registers the plan observe-only: the engine evaluates and
///   advances it but never dispatches its fires to the broker (see
///   [`TradePlan::shadow`](trade_control_core::trade_plan::TradePlan::shadow)).
///   The safe way to diff the engine against the live TV alerts on demo.
/// - `replay_start` is the arm-time `--start` cursor (a Unix second), baked onto
///   the plan so the offline `replay-candles` harness derives a self-consistent
///   window without reading the TV chart's replay cursor. `None` when `--start`
///   wasn't passed (see
///   [`TradePlan::replay_start`](trade_control_core::trade_plan::TradePlan::replay_start)).
// - `retest_atr_step` is the per-bar ATR-multiple decay of the retest tolerance
//   (`tv-arm --retest-atr-step`, default
//   [`DEFAULT_RETEST_ATR_STEP`](trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP)),
//   baked onto the plan's `retest_atr_step`.
// - `trend` is the `tv-arm --trend` arm (see [`TrendFollow`]): it makes the
//   pcl-exhausted veto close-confirmed rather than wick-triggered. Not a plan
//   field — it only selects the trigger's `BarEvent`, so it rides the signed
//   plan as the trigger it produced and needs no engine change.
// - `armed_at` is the arm-time wall-clock (`Utc::now()` from the pipeline),
//   baked onto the plan for read-back only (see
//   [`TradePlan::armed_at`](trade_control_core::trade_plan::TradePlan::armed_at)).
// - `armed_sentiment` is the news-sentiment verdict as of `armed_at`, likewise
//   baked for journalling only; `None` when it couldn't be computed (arming
//   never blocks on it). See
//   [`TradePlan::armed_sentiment`](trade_control_core::trade_plan::TradePlan::armed_sentiment).
// - `screenshot_url` is the TradingView snapshot link read off the clipboard at
//   arm time, likewise baked for journalling only; `None` when the clipboard
//   held no such URL (arming never depends on it). See
//   [`TradePlan::screenshot_url`](trade_control_core::trade_plan::TradePlan::screenshot_url).
//
// Each parameter is a distinct chart-derived primitive (id, instrument, alerts,
// direction, roles, granularity, is_mw, shadow, replay_start, retest_atr_step,
// cross_buffer_pct, cross_buffer_atr, armed_at, armed_sentiment, screenshot_url)
// threaded once from the single pipeline call site. Grouping them into a struct
// would just move the same fields elsewhere without clarifying anything.
#[allow(clippy::too_many_arguments)]
pub fn build_trade_plan(
    trade_id: &str,
    instrument: &str,
    alerts: &[BuiltAlert],
    direction: ConvDirection,
    geom: &PlanGeometry,
    granularity: Granularity,
    is_mw: bool,
    shadow: bool,
    replay_start: Option<i64>,
    retest_atr_step: f64,
    cross_buffer_pct: f64,
    cross_buffer_atr: f64,
    bcr_require_golden: bool,
    armed_at: chrono::DateTime<chrono::Utc>,
    armed_sentiment: Option<trade_control_core::plan_sentiment::PlanSentiment>,
    pullback_arm: Option<PullbackArm>,
    screenshot_url: Option<trade_control_core::screenshot::ScreenshotUrl>,
    trend: TrendFollow,
) -> TradePlan {
    let rules = alerts
        .iter()
        .filter_map(|alert| {
            build_rule(
                alert,
                direction,
                geom,
                granularity,
                is_mw,
                pullback_arm,
                trend,
            )
        })
        .collect();

    TradePlan {
        trade_id: trade_id.to_string(),
        instrument: instrument.to_string(),
        direction: to_core_direction(direction),
        granularity,
        pip_size: pip_size_of(alerts),
        rules,
        shadow,
        cross_buffer_pct,
        cross_buffer_atr,
        bcr_require_golden,
        retest_atr_step,
        replay_start,
        armed_at: Some(armed_at),
        armed_sentiment,
        screenshot_url,
    }
}

/// Append the pause/news/calendar **control bars** to a built plan as
/// `TimeReached` rules — one per bundle alert, carrying that alert's embedded
/// intent verbatim and firing at the bundle's window edge (start for
/// pause-start / news-start, end for pause-resume / news-end).
///
/// This is what makes `--register-plan` open/close the same blackout + news
/// windows the `--create-alerts` path POSTs as TradingView alerts. Since PR1b
/// the windows always arrive as [`BuiltPause`]/[`BuiltNews`] bundles — sourced
/// from the calendar directly (`calendar_windows` in `pipeline.rs`), not from
/// drawn lines. Both feed the same per-alert conversion.
///
/// `build_trade_plan`'s `trigger_for` deliberately does **not** handle these
/// basenames anymore: it only ever saw `roles.*_pairs.first()` (one pair) and
/// the control alerts were never in `built_trade.alerts` to begin with — so the
/// rules came from here, where every window is represented.
pub fn append_control_rules(
    plan: &mut TradePlan,
    pause_bundles: &[&BuiltPause],
    news_bundles: &[&BuiltNews],
) {
    for b in pause_bundles {
        push_window_rules(plan, &b.alerts, b.start_time, b.end_time);
    }
    for b in news_bundles {
        push_window_rules(plan, &b.alerts, b.start_time, b.end_time);
    }
}

/// Turn one window's built alerts into `TimeReached` rules on the plan. Each
/// alert exposes a `basename` + the signed `intent`; the basename selects which
/// window edge (`start`/`end`) the rule's epoch anchors to. An unrecognised
/// basename is skipped (it isn't a window-edge control alert).
fn push_window_rules<A: WindowAlert>(
    plan: &mut TradePlan,
    alerts: &[A],
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
) {
    for alert in alerts {
        let Some(basename) = AlertBasename::parse(alert.basename()) else {
            continue;
        };
        let at_epoch = match basename {
            AlertBasename::PauseStart(_) | AlertBasename::NewsStart(_) => start.timestamp(),
            AlertBasename::PauseResume(_) | AlertBasename::NewsEnd(_) => end.timestamp(),
            _ => continue,
        };
        plan.rules.push(ConditionRule {
            rule_id: alert.basename().to_string(),
            trigger: Trigger::TimeReached { at_epoch },
            fire_mode: FireMode::Once,
            intent: alert.intent().clone(),
            kind: RuleKind::from(&basename),
        });
    }
}

/// A built window alert (pause/news): a basename + the signed intent. Lets
/// [`push_window_rules`] treat [`BuiltPauseAlert`](trade_control_cli::BuiltPauseAlert)
/// and [`BuiltNewsAlert`](trade_control_cli::BuiltNewsAlert) uniformly — they
/// have identical shape but are distinct types.
trait WindowAlert {
    fn basename(&self) -> &str;
    fn intent(&self) -> &Intent;
}

impl WindowAlert for trade_control_cli::BuiltPauseAlert {
    fn basename(&self) -> &str {
        &self.basename
    }
    fn intent(&self) -> &Intent {
        &self.intent
    }
}

impl WindowAlert for trade_control_cli::BuiltNewsAlert {
    fn basename(&self) -> &str {
        &self.basename
    }
    fn intent(&self) -> &Intent {
        &self.intent
    }
}

/// One [`BuiltAlert`] → one [`ConditionRule`], or `None` when the role the
/// trigger needs isn't on the chart. The embedded intent is cloned verbatim
/// from the built alert — it is the exact action the TV alert would have
/// POSTed.
fn build_rule(
    alert: &BuiltAlert,
    direction: ConvDirection,
    geom: &PlanGeometry,
    granularity: Granularity,
    is_mw: bool,
    pullback_arm: Option<PullbackArm>,
    trend: TrendFollow,
) -> Option<ConditionRule> {
    let basename = AlertBasename::parse(&alert.basename)?;
    let trigger = trigger_for(
        &basename,
        direction,
        geom,
        granularity,
        is_mw,
        pullback_arm,
        trend,
    )?;
    let fire_mode = fire_mode_for(&trigger);
    let kind = RuleKind::from(&basename);
    Some(ConditionRule {
        rule_id: alert.basename.clone(),
        trigger,
        fire_mode,
        intent: alert.intent.clone(),
        kind,
    })
}

/// The 1:1 port of `build_alert_spec`'s basename → condition dispatch,
/// re-expressed as a [`Trigger`]. Returns `None` for a missing role (same
/// skip semantics) or a basename with no server-side trigger.
fn trigger_for(
    basename: &AlertBasename,
    direction: ConvDirection,
    geom: &PlanGeometry,
    granularity: Granularity,
    is_mw: bool,
    pullback_arm: Option<PullbackArm>,
    trend: TrendFollow,
) -> Option<Trigger> {
    match basename {
        AlertBasename::VetoTooHigh | AlertBasename::VetoTooLow => {
            invalidation_or_pcl_trigger(basename, direction, geom, trend)
        }
        // Trade-expiry / prep-expiry are vertical-line time triggers. The veto
        // fires when wall-clock reaches the line.
        AlertBasename::VetoTradeExpiry => time_trigger(geom.trade_expiry_epoch),
        AlertBasename::PrepExpire(step) => time_trigger(geom.prep_expiry(step)),
        // Pause / news are control bars: they're folded into the plan from the
        // built pause/news/calendar bundles by [`append_control_rules`], not
        // from `built_trade.alerts` (these basenames never appear there), so
        // they are not handled here. A `built_trade` alert with one of these
        // basenames (there is none) would be skipped.
        AlertBasename::PauseStart(_)
        | AlertBasename::PauseResume(_)
        | AlertBasename::NewsStart(_)
        | AlertBasename::NewsEnd(_) => None,
        // Break-and-close: neckline trendline, closes through it. Short closes
        // down, long closes up — same as the TV `CrossDown`/`CrossUp`.
        AlertBasename::PrepBreakAndClose => trendline_trigger(
            geom.neckline,
            close_dir(direction),
            BarEvent::OnClose,
            granularity,
        ),
        // Retest: opposite cross of the SAME neckline trendline, intrabar. Reading
        // one `geom.neckline` for both is not a simplification — `roles::classify`
        // assigns `roles.retest = break_and_close.clone()` unconditionally (a
        // separately-drawn retest line is deliberately ignored, since an old
        // extrapolated anchor could make the retest uncrossable). So the two reads
        // were already the same drawing.
        AlertBasename::PrepRetest => trendline_trigger(
            geom.neckline,
            retest_dir(direction),
            BarEvent::Intrabar,
            granularity,
        ),
        // Pullback: ≥N×ATR body retrace since arm time. No drawing — the anchor
        // (arm-time mid open) and the ATR multiple are the arm-time `PullbackArm`,
        // baked onto the trigger. Skipped (no rule) if the arm didn't request it.
        AlertBasename::PrepPullback => pullback_arm.map(|pb| Trigger::PullbackFromArm {
            anchor_open: pb.anchor_open,
            atr_mult: pb.atr_mult,
            dir: to_core_direction(direction),
        }),
        // Enter: H&S binds to the direction's candle pattern; M/W to the
        // per-bar geometry heartbeat. The strategy-v2 Quasimodo enter
        // (`EnterQm`) is H&S-only and decided by the *same* candle detector as
        // `Enter` — the difference between the two is the intent (no preps,
        // limit order), not the trigger. So it maps to the same PinePattern.
        AlertBasename::Enter | AlertBasename::EnterQm => Some(if is_mw {
            Trigger::MwEveryBar
        } else {
            Trigger::PinePattern {
                pattern: None,
                dir: to_core_direction(direction),
            }
        }),
        // Close-on-reversal binds to the *opposite* direction's pattern.
        AlertBasename::CloseOnReversal | AlertBasename::CloseOnSrReversal => {
            Some(Trigger::PinePattern {
                pattern: None,
                dir: to_core_direction(direction.opposite()),
            })
        }
        // M/W price-level vetos from the path anchors [A, B, C].
        AlertBasename::VetoMwCancel => mw_price_trigger(geom, MwVeto::Cancel),
        AlertBasename::VetoMwAbort => mw_price_trigger(geom, MwVeto::Abort),
        AlertBasename::VetoMwOvershoot => mw_price_trigger(geom, MwVeto::Overshoot),
    }
}

/// Which M/W price-level veto — mirrors `alert_spec::MwVeto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MwVeto {
    Cancel,
    Abort,
    Overshoot,
}

/// Invalidation (drawing-bound horizontal) when the basename matches the
/// trade's natural invalidation direction, else the pcl-exhausted price-value
/// veto from the fib. Verbatim port of `alert_spec::invalidation_or_pcl`.
fn invalidation_or_pcl_trigger(
    basename: &AlertBasename,
    direction: ConvDirection,
    geom: &PlanGeometry,
    trend: TrendFollow,
) -> Option<Trigger> {
    let basename_dir = match basename {
        AlertBasename::VetoTooHigh => ConvDirection::Short,
        AlertBasename::VetoTooLow => ConvDirection::Long,
        _ => return None,
    };
    if basename_dir == direction {
        // Drawing-bound invalidation = the human's **drawn line** (below the
        // shoulder/head for a long `too-low`, above for a short `too-high`).
        //
        // A drawn line is **close-confirmed** (`OnClose`) in *both* directions:
        // the operator's semantics are "the candle opened one side of my line
        // and closed the other" — a genuine break. An intrabar spike through the
        // line that closes back does not invalidate. This is the line-vs-fib
        // distinction (operator 2026-07-01): the drawn line is close-confirm;
        // the fib level (the `else` branch) is a wick-through. Direction only
        // decides which *way* the line is crossed, not the confirm mode.
        let dir = match direction {
            ConvDirection::Short => CrossDir::Up,  // close above the cap
            ConvDirection::Long => CrossDir::Down, // close below the floor
        };
        Some(Trigger::HorizontalCross {
            level: geom.invalidation?,
            dir,
            bar: BarEvent::OnClose,
        })
    } else {
        // Opposite-name veto = pcl-exhausted, a computed **fib** level ("the
        // power of the setup has been consumed"). A fib level is normally a
        // **wick-through** (`Intrabar`, `Either`): any straddle aborts — if the
        // move ran ~80% to TP without us, a wick alone is reason enough.
        //
        // `--trend` flips it to `OnClose`. A trend-continuation setup runs
        // further and wicks deeper than a reversal, so the straddle aborts on a
        // spike that closes back inside — which is what killed the 2026-08-07
        // short (`01-veto-too-low` off a wick, ~30 min after arming). Under
        // `--trend` the bar must *close* past the level. `CrossDir::Either`
        // carries over unchanged: the engine's settled/origin `OnClose` arm
        // reads `Either` as "origin one side, close past the opposite far
        // edge", which is exactly "closed through the level" for both
        // directions — so this needs no direction handling here and no engine
        // change.
        //
        // Head/neckline were already resolved via the fib's `reverse` flag (not
        // point order) when the geometry was extracted.
        let (head, neckline) = geom.fib_head_neckline?;
        Some(Trigger::PriceValueCross {
            level: pcl_exhausted_price(head, neckline),
            dir: CrossDir::Either,
            bar: trend.pcl_bar_event(),
        })
    }
}

/// Build an M/W cancel / abort / overshoot price-value trigger from the path
/// anchors. Verbatim port of `alert_spec::mw_price_veto`.
fn mw_price_trigger(geom: &PlanGeometry, which: MwVeto) -> Option<Trigger> {
    let path = geom.mw_path?;
    // 4-point path: anchor the cancel / overshoot levels to the **higher** of
    // the two drawn shoulders, so a drawn right shoulder above the left widens
    // the 1.3 cancel ceiling and pushes the overshoot level out to match the
    // real geometry. The abort (neckline close) is shoulder-independent.
    let (first_point, neckline) = (path.first_point, path.neckline);
    let shoulder = highest_shoulder(first_point, neckline, path.right_shoulder);
    let (level, bar) = match which {
        MwVeto::Cancel => (cancel_level(shoulder, neckline), BarEvent::Intrabar),
        // Abort is the only M/W veto that's a candle *close* back through the
        // neckline → OnClose.
        MwVeto::Abort => (abort_level(neckline), BarEvent::OnClose),
        MwVeto::Overshoot => (overshoot_level(shoulder, neckline), BarEvent::Intrabar),
    };
    Some(Trigger::PriceValueCross {
        level,
        dir: CrossDir::Either,
        bar,
    })
}

/// A vertical-line time trigger from an epoch, or `None` if the geometry has no
/// such marker (an undrawn expiry → no rule, as before).
fn time_trigger(at_epoch: Option<i64>) -> Option<Trigger> {
    Some(Trigger::TimeReached {
        at_epoch: at_epoch?,
    })
}

/// A trendline cross trigger from a two-anchor line. Necklines are
/// extended forward so a cross past the right anchor still fires (the engine
/// analogue of the TV `extend_forward` flag — see the README trendline note).
fn trendline_trigger(
    line: Option<Line>,
    dir: CrossDir,
    bar: BarEvent,
    granularity: Granularity,
) -> Option<Trigger> {
    let line = line?;
    Some(Trigger::TrendlineCross {
        a: line.a.to_line_point(),
        b: line.b.to_line_point(),
        extend_forward: true,
        // The engine interpolates the line in bar-index space; this is the
        // nominal bar duration it falls back to when an anchor predates the
        // fetched candle window (see `Trigger::TrendlineCross::bar_seconds`).
        bar_seconds: granularity.seconds(),
        dir,
        bar,
    })
}

/// Break-and-close cross direction: short closes *down* through the neckline,
/// long closes *up*.
fn close_dir(direction: ConvDirection) -> CrossDir {
    match direction {
        ConvDirection::Short => CrossDir::Down,
        ConvDirection::Long => CrossDir::Up,
    }
}

/// Retest cross direction: the opposite of the break-and-close cross.
fn retest_dir(direction: ConvDirection) -> CrossDir {
    match direction {
        ConvDirection::Short => CrossDir::Up,
        ConvDirection::Long => CrossDir::Down,
    }
}

/// Fire-once for everything except the M/W per-bar heartbeat, which
/// re-evaluates the geometry every bar. The stateful engine latches every
/// other rule after its first fire (unlike a TV `OnFirstFire` alert that
/// re-fires on each touch).
fn fire_mode_for(trigger: &Trigger) -> FireMode {
    match trigger {
        Trigger::MwEveryBar => FireMode::EveryBar,
        _ => FireMode::Once,
    }
}

/// The instrument pip size to bake on the plan: read it from the enter
/// intent (the authoritative baked value from `instrument-lookup`), falling
/// back to the forex default if somehow absent.
fn pip_size_of(alerts: &[BuiltAlert]) -> f64 {
    alerts
        .iter()
        .find(|a| a.basename == "05-enter")
        .and_then(|a| a.intent.pip_size)
        .or_else(|| alerts.iter().find_map(|a| a.intent.pip_size))
        .unwrap_or(0.0001)
}

/// Convert the conventions `Direction` (used across tv-arm) to the core
/// `Direction` the `TradePlan` carries. Both are plain `Long`/`Short`.
fn to_core_direction(d: ConvDirection) -> Direction {
    match d {
        ConvDirection::Long => Direction::Long,
        ConvDirection::Short => Direction::Short,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The tests deliberately still build `Roles` (i.e. real chart drawings) and
    // pass them through `PlanGeometry::from_roles`. That keeps them a test of the
    // WHOLE drawings -> geometry -> plan path, so the extraction can't silently
    // change what a chart produces. Testing `PlanGeometry` directly would only
    // prove the second half.
    use crate::roles::Roles;

    /// Build the plan the way the pipeline does: extract geometry from the chart
    /// roles, then build. Keeps every test's call site honest about the seam.
    fn geom_of(roles: &Roles) -> PlanGeometry {
        PlanGeometry::from_roles(roles)
    }

    #[test]
    fn resolution_maps_known_timeframes() {
        assert_eq!(resolution_to_granularity("1"), Some(Granularity::M1));
        assert_eq!(resolution_to_granularity("5"), Some(Granularity::M5));
        assert_eq!(resolution_to_granularity("15"), Some(Granularity::M15));
        assert_eq!(resolution_to_granularity("60"), Some(Granularity::H1));
        assert_eq!(resolution_to_granularity("240"), Some(Granularity::H4));
        assert_eq!(resolution_to_granularity("D"), Some(Granularity::D1));
        assert_eq!(resolution_to_granularity(" 60 "), Some(Granularity::H1));
    }

    #[test]
    fn resolution_rejects_unsupported() {
        assert_eq!(resolution_to_granularity("3"), None);
        assert_eq!(resolution_to_granularity("W"), None);
        assert_eq!(resolution_to_granularity(""), None);
    }

    #[test]
    fn close_and_retest_dirs_are_opposite() {
        assert_eq!(close_dir(ConvDirection::Short), CrossDir::Down);
        assert_eq!(retest_dir(ConvDirection::Short), CrossDir::Up);
        assert_eq!(close_dir(ConvDirection::Long), CrossDir::Up);
        assert_eq!(retest_dir(ConvDirection::Long), CrossDir::Down);
    }

    #[test]
    fn fire_mode_latches_except_mw_heartbeat() {
        assert_eq!(fire_mode_for(&Trigger::MwEveryBar), FireMode::EveryBar);
        assert_eq!(
            fire_mode_for(&Trigger::HorizontalCross {
                level: 1.0,
                dir: CrossDir::Up,
                bar: BarEvent::Intrabar,
            }),
            FireMode::Once
        );
    }

    // ===== Full build_trade_plan port checks =====

    use chrono::{DateTime, Utc};
    use trade_control_core::intent::{Action, Intent};
    use trade_control_core::tunable::Tunable;
    use trading_view::drawings::{Drawing, Point, Properties};

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// A bare intent carrying just what `build_trade_plan` reads
    /// (action/pip_size) — the rest is irrelevant to trigger mapping and is
    /// copied verbatim into the rule.
    fn intent(action: Action, pip_size: Option<f64>) -> Intent {
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
            broker: trade_control_core::intent::BrokerKind::Oanda,
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
            pip_size,
            tick_size: None,
            spread_window: None,
            trade_plan: None,
            blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
            breakeven: None,
            include_archived: false,
        }
    }

    fn alert(basename: &str, action: Action) -> BuiltAlert {
        BuiltAlert {
            basename: basename.into(),
            purpose: String::new(),
            intent: intent(action, Some(0.0001)),
        }
    }

    fn horz(price: f64) -> Drawing {
        Drawing {
            id: "h".into(),
            points: vec![Point { time: 1, price }],
            properties: Properties::default(),
        }
    }

    fn trend(a: (i64, f64), b: (i64, f64)) -> Drawing {
        Drawing {
            id: "t".into(),
            points: vec![
                Point {
                    time: a.0,
                    price: a.1,
                },
                Point {
                    time: b.0,
                    price: b.1,
                },
            ],
            properties: Properties::default(),
        }
    }

    fn vert(time: i64) -> Drawing {
        Drawing {
            id: "v".into(),
            points: vec![Point { time, price: 0.0 }],
            properties: Properties::default(),
        }
    }

    /// A multi-anchor `path` drawing — the M/W form (3 anchors, or 4 with a drawn
    /// right shoulder).
    fn path(points: &[(i64, f64)]) -> Drawing {
        Drawing {
            id: "p".into(),
            points: points
                .iter()
                .map(|&(time, price)| Point { time, price })
                .collect(),
            properties: Properties::default(),
        }
    }

    /// **The point of the `PlanGeometry` seam.** A plan built from geometry that
    /// has been through a JSON round-trip — i.e. frozen to disk and reloaded, with
    /// no `Drawing` and no TradingView anywhere — is **identical** to one built
    /// from the live chart drawings.
    ///
    /// That equality is what makes a re-armable spec possible: freeze the geometry
    /// once (operator confirms the right pattern), and every later rebuild is
    /// reproducible and can't pick a different drawing off the chart.
    /// The same parity claim, but through an actual **`--spec-out` file**:
    /// write, reload from disk, rebuild, compare.
    ///
    /// The sibling test below round-trips `PlanGeometry` through a JSON *string*.
    /// This one goes through `FrozenSetup::write` → `load`, which is the step a
    /// real `--spec-in` arm performs — a different code path with its own
    /// `deny_unknown_fields`, its own version gate, and its own set of
    /// `skip_serializing_if` attributes, any of which could drop a field that an
    /// in-memory compare never sees.
    ///
    /// Same caveat as its sibling, stated so nobody over-trusts it: a field
    /// `PlanGeometry` never carried is absent on both sides and this still
    /// passes. The key-set guards are what cover that — and they have caught
    /// three real dropped fields (`runup_start`, `sr_levels`, `anchors`).
    #[test]
    fn a_plan_from_a_spec_written_to_disk_matches_the_live_one() {
        let alerts = vec![
            alert("01-veto-too-high", Action::Veto),
            alert("03-prep-break-and-close", Action::Prep),
            alert("04-prep-retest", Action::Prep),
            alert("02-veto-trade-expiry", Action::Invalidate),
            alert("05-enter", Action::Enter),
        ];
        let roles = Roles {
            invalidation: Some(horz(1.2000)),
            break_and_close: Some(trend((10, 1.1900), (20, 1.1850))),
            retest: Some(trend((10, 1.1900), (20, 1.1850))),
            trade_expiry: Some(vert(99_000)),
            tp_fib: Some(trend((10, 1.1700), (20, 1.1900))),
            ..Roles::default()
        };
        let live = PlanGeometry::from_roles(&roles);

        // Freeze to a real file and read it back — the `--spec-out` /
        // `--spec-in` round trip.
        let path = std::env::temp_dir().join(format!("spec-parity-{}.json", std::process::id()));
        crate::frozen_setup::FrozenSetup::capture(
            live.clone(),
            "60".into(),
            "OANDA:EUR_USD".into(),
            Some(1_700_000_000),
            None,
        )
        .write(&path)
        .expect("write spec");
        let reloaded = crate::frozen_setup::FrozenSetup::load(&path).expect("load spec");
        std::fs::remove_file(&path).ok();

        assert_eq!(
            reloaded.geom, live,
            "geometry must survive the file round-trip"
        );
        assert_eq!(
            reloaded.resolution, "60",
            "the granularity must survive — losing it reprices the whole neckline"
        );

        let build = |geom: &PlanGeometry| {
            build_trade_plan(
                "eurusd-hs-spec",
                "EUR_USD",
                &alerts,
                ConvDirection::Short,
                geom,
                Granularity::H1,
                false,
                false,
                None,
                trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
                trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
                trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
                false,
                chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid"),
                None,
                None,
                None, // screenshot_url
                TrendFollow::REVERSAL,
            )
        };
        assert_eq!(
            serde_json::to_value(build(&live)).expect("live"),
            serde_json::to_value(build(&reloaded.geom)).expect("reloaded"),
            "a plan armed from a spec file must be identical to the live one"
        );
    }

    #[test]
    fn a_plan_built_from_frozen_geometry_matches_one_built_from_drawings() {
        let alerts = vec![
            alert("01-veto-too-high", Action::Veto),
            alert("01-veto-too-low", Action::Veto),
            alert("03-prep-break-and-close", Action::Prep),
            alert("04-prep-retest", Action::Prep),
            alert("02-veto-trade-expiry", Action::Invalidate),
            alert("05-enter", Action::Enter),
        ];
        let roles = Roles {
            invalidation: Some(horz(1.2000)),
            break_and_close: Some(trend((10, 1.1900), (20, 1.1850))),
            retest: Some(trend((10, 1.1900), (20, 1.1850))),
            trade_expiry: Some(vert(99_000)),
            // A fib is read as two anchor prices resolved through its `reverse`
            // flag; `trend` gives the same two-point shape (reverse unset =>
            // false => head is points[1]).
            tp_fib: Some(trend((10, 1.1700), (20, 1.1900))),
            ..Roles::default()
        };

        let build = |geom: &PlanGeometry| {
            build_trade_plan(
                "eurusd-hs-frozen",
                "EUR_USD",
                &alerts,
                ConvDirection::Short,
                geom,
                Granularity::H1,
                false,
                false,
                None,
                trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
                trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
                trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
                false,
                chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
                None,
                None,
                None, // screenshot_url
                TrendFollow::REVERSAL,
            )
        };

        let live = PlanGeometry::from_roles(&roles);
        // Freeze -> reload. This is the step a `--spec-in` re-arm performs.
        let frozen: PlanGeometry =
            serde_json::from_str(&serde_json::to_string(&live).unwrap()).unwrap();
        assert_eq!(live, frozen, "geometry must survive the round-trip");

        // `TradePlan` has no `PartialEq`, so compare the serialized form — the same
        // equality the fixture goldens use.
        assert_eq!(
            serde_json::to_value(build(&live)).unwrap(),
            serde_json::to_value(build(&frozen)).unwrap(),
            "a plan rebuilt from frozen geometry must be byte-identical"
        );

        // ⚠ The two assertions above are NECESSARY BUT NOT SUFFICIENT, and it's
        // worth being explicit about why: both sides call the same `build` on
        // values already asserted equal, so it is `f(x) == f(x)`. A field that
        // `from_roles` silently drops disappears from BOTH sides and the test
        // still passes — which is exactly what happened with `MwPath.runup_start`
        // (fixed in 4713acb; this test would not have caught it).
        //
        // So the real check is below: every role the chart supplied must be
        // PRESENT, with the right VALUE, in the extracted geometry.
        assert_eq!(
            live.invalidation,
            Some(1.2000),
            "invalidation lost/mis-wired"
        );
        assert_eq!(
            live.trade_expiry_epoch,
            Some(99_000),
            "trade-expiry epoch lost/mis-wired"
        );
        let (head, neck) = live.fib_head_neckline.expect("fib anchors lost");
        assert_eq!(
            (head, neck),
            (1.1900, 1.1700),
            "fib head/neckline must resolve through the `reverse` flag: head is \
             points[1] when reverse is unset"
        );
        let neckline = live.neckline.expect("neckline lost");
        assert_eq!(
            (neckline.a.price, neckline.b.price),
            (1.1900, 1.1850),
            "neckline anchors must carry the drawn prices in order"
        );
        assert_eq!(
            (neckline.a.at_epoch, neckline.b.at_epoch),
            (10, 20),
            "…and their epochs, which is what bar-index interpolation needs"
        );
    }

    /// The complement to the test above, and the one that actually guards the
    /// `--spec-in` promise: a geometry extracted from a chart with **every** role
    /// present must serialize **every** field.
    ///
    /// Asserted against the serialized key set rather than field-by-field, so a
    /// NEWLY ADDED field is caught too — it'll be absent from the expected list
    /// and force whoever adds it to decide whether it belongs in the frozen
    /// contract. That's the check `runup_start` needed.
    #[test]
    fn a_fully_drawn_chart_freezes_every_geometry_field() {
        let roles = Roles {
            invalidation: Some(horz(1.2000)),
            invalidation_label: Some("too-high".into()),
            break_and_close: Some(trend((10, 1.1900), (20, 1.1850))),
            retest: Some(trend((10, 1.1900), (20, 1.1850))),
            trade_expiry: Some(vert(99_000)),
            tp_fib: Some(trend((10, 1.1700), (20, 1.1900))),
            prep_expiries: vec![("retest".to_string(), vert(98_000))],
            // Two drawn S/R horizontals. Load-bearing for THIS test: `sr_levels`
            // is `skip_serializing_if = "Vec::is_empty"`, so leaving them off
            // would omit the key and the field would slip past the key-set
            // assertion — the exact hole that let `runup_start` through.
            sr_levels: vec![horz(1.1950), horz(1.1600)],
            ..Roles::default()
        };
        // The M/W half. H&S and M/W are mutually exclusive on a chart (an H&S has
        // no path drawing, an M/W has no fib/invalidation), so no SINGLE chart
        // populates every field — the contract is covered by their union.
        let mw_roles = Roles {
            mw_path: Some(path(&[(10, 1.1000), (20, 1.2000), (30, 1.1500)])),
            trade_expiry: Some(vert(99_000)),
            ..Roles::default()
        };

        let keys_of = |r: &Roles| -> std::collections::BTreeSet<String> {
            serde_json::to_value(PlanGeometry::from_roles(r))
                .expect("serialize")
                .as_object()
                .expect("an object")
                .keys()
                .cloned()
                .collect()
        };
        let mut reachable = keys_of(&roles);
        reachable.extend(keys_of(&mw_roles));

        // Every field of the frozen contract. Update ONLY when you have decided
        // whether a new field is part of what a `--spec-in` re-arm must restore.
        let expected: std::collections::BTreeSet<String> = [
            "neckline",
            "invalidation",
            "fib_head_neckline",
            "trade_expiry_epoch",
            "prep_expiry_epochs",
            "mw_path",
            // Decided YES, 2026-07-27: a re-arm must restore the drawn S/R
            // levels. They gate whether `07-close-on-sr-reversal` is armed, and
            // the failure without them is silent — the derived TP band keeps the
            // vec non-empty, so the alert still fires, just off the wrong levels.
            "sr_levels",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        assert_eq!(
            reachable, expected,
            "PlanGeometry's frozen field set changed. If you ADDED a field, decide \
             whether a re-arm must restore it, then update this list. If a field \
             VANISHED (unreachable from BOTH an H&S and an M/W chart), `from_roles` \
             stopped reading a role — that's the `runup_start` bug class."
        );

        // And the fields really are populated, so the key set above isn't passing
        // on the strength of serialized `None`s.
        let hs = PlanGeometry::from_roles(&roles);
        assert!(hs.neckline.is_some());
        assert!(hs.invalidation.is_some());
        assert!(hs.fib_head_neckline.is_some());
        assert!(hs.trade_expiry_epoch.is_some());
        assert_eq!(hs.prep_expiry_epochs.len(), 1);
        assert!(
            hs.mw_path.is_none(),
            "an H&S chart has no path drawing — the M/W key is legitimately absent"
        );

        let mw = PlanGeometry::from_roles(&mw_roles);
        let mw_path = mw.mw_path.expect("M/W path extracted");
        // `runup_start` specifically: the field whose loss this whole test class
        // exists to catch. Direction and two gates read it.
        assert_eq!(mw_path.runup_start, 1.1000);
        assert_eq!(mw_path.first_point, 1.2000);
        assert_eq!(mw_path.neckline, 1.1500);
        assert!(
            mw_path.right_shoulder.is_none(),
            "a 3-anchor path has no drawn right shoulder"
        );
    }

    /// A short H&S trade folds its invalidation / break-and-close / retest /
    /// trade-expiry / enter alerts into the matching triggers, carrying each
    /// embedded intent verbatim and latching every rule but (here) none being
    /// the M/W heartbeat.
    #[test]
    fn builds_hs_short_rules_with_correct_triggers() {
        let alerts = vec![
            alert("01-veto-too-high", Action::Veto),
            alert("03-prep-break-and-close", Action::Prep),
            alert("04-prep-retest", Action::Prep),
            alert("02-veto-trade-expiry", Action::Invalidate),
            alert("05-enter", Action::Enter),
        ];
        let roles = Roles {
            invalidation: Some(horz(1.2000)),
            break_and_close: Some(trend((10, 1.1900), (20, 1.1850))),
            retest: Some(trend((10, 1.1900), (20, 1.1850))),
            trade_expiry: Some(vert(99_000)),
            ..Roles::default()
        };

        let plan = build_trade_plan(
            "eurusd-hs-1",
            "EUR_USD",
            &alerts,
            ConvDirection::Short,
            &geom_of(&roles),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );

        assert!(!plan.shadow, "default build is live, not shadow");
        assert_eq!(plan.trade_id, "eurusd-hs-1");
        assert_eq!(plan.granularity, Granularity::H1);
        assert_eq!(plan.direction, Direction::Short);
        assert_eq!(plan.pip_size, 0.0001);
        assert_eq!(plan.rules.len(), 5);

        let by_id = |id: &str| plan.rules.iter().find(|r| r.rule_id == id).unwrap();

        // Invalidation: short crosses UP into the cap, **close-confirmed**
        // (`OnClose`) — the literal `too-high` cap must close above to
        // invalidate; a spike-and-recover does not. Fire-once.
        assert!(matches!(
            by_id("01-veto-too-high").trigger,
            Trigger::HorizontalCross {
                level,
                dir: CrossDir::Up,
                bar: BarEvent::OnClose,
            } if (level - 1.2000).abs() < 1e-9
        ));
        // Break-and-close: short closes DOWN through the neckline, OnClose.
        // `bar_seconds` is baked from the H1 chart granularity (3600s) so the
        // engine can fall back to a bar-spacing divisor if an anchor predates
        // its fetched candle window.
        assert!(matches!(
            by_id("03-prep-break-and-close").trigger,
            Trigger::TrendlineCross {
                dir: CrossDir::Down,
                bar: BarEvent::OnClose,
                extend_forward: true,
                bar_seconds: 3600,
                ..
            }
        ));
        // Retest: opposite cross (UP), intrabar.
        assert!(matches!(
            by_id("04-prep-retest").trigger,
            Trigger::TrendlineCross {
                dir: CrossDir::Up,
                bar: BarEvent::Intrabar,
                ..
            }
        ));
        // Trade-expiry: time reached at the vertical's epoch.
        assert!(matches!(
            by_id("02-veto-trade-expiry").trigger,
            Trigger::TimeReached { at_epoch: 99_000 }
        ));
        // Enter (H&S): the short pattern, fire-once.
        let enter = by_id("05-enter");
        assert!(matches!(
            enter.trigger,
            Trigger::PinePattern {
                pattern: None,
                dir: Direction::Short,
            }
        ));
        assert_eq!(enter.fire_mode, FireMode::Once);
    }

    #[test]
    fn pullback_alert_builds_pullback_from_arm_trigger_with_baked_anchor() {
        // A 04b-prep-pullback alert + a PullbackArm ⇒ a PullbackFromArm trigger
        // carrying the baked anchor mid + ATR multiple + trade direction.
        let alerts = vec![alert("04b-prep-pullback", Action::Prep)];
        let roles = Roles::default();
        let plan = build_trade_plan(
            "eurusd-hs-pb",
            "EUR_USD",
            &alerts,
            ConvDirection::Short,
            &geom_of(&roles),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false,
            chrono::Utc::now(),
            None,
            Some(PullbackArm {
                anchor_open: 1.2345,
                atr_mult: 1.5,
            }),
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );
        assert_eq!(plan.rules.len(), 1);
        assert!(matches!(
            plan.rules[0].trigger,
            Trigger::PullbackFromArm {
                anchor_open,
                atr_mult,
                dir: Direction::Short,
            } if (anchor_open - 1.2345).abs() < 1e-9 && (atr_mult - 1.5).abs() < 1e-9
        ));
    }

    #[test]
    fn pullback_alert_without_arm_yields_no_rule() {
        // The alert is present but no PullbackArm was captured (nothing armed) —
        // the rule is skipped (same Ok(None) semantics as a missing role).
        let alerts = vec![alert("04b-prep-pullback", Action::Prep)];
        let plan = build_trade_plan(
            "eurusd-hs-pb",
            "EUR_USD",
            &alerts,
            ConvDirection::Short,
            &PlanGeometry::from_roles(&Roles::default()),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false,
            chrono::Utc::now(),
            None,
            None, // no pullback arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );
        assert_eq!(plan.rules.len(), 0);
    }

    /// The long-side (IH&S) invalidation floor is a **drawn line** (named
    /// `01-veto-too-low`), so it is **close-confirmed** (`OnClose`) — a bar must
    /// open above and *close* below the floor to invalidate; an intrabar wick
    /// through that closes back does not. This is the line-vs-fib rule (operator
    /// 2026-07-01): the human's drawn line is close-confirm in *both* directions;
    /// only the computed fib/pcl level is a wick-through. (Supersedes the earlier
    /// asymmetry where only the short `too-high` cap was close-confirm.)
    #[test]
    fn ihs_long_too_low_invalidation_is_close_confirmed() {
        let alerts = vec![
            alert("01-veto-too-low", Action::Veto),
            alert("03-prep-break-and-close", Action::Prep),
            alert("04-prep-retest", Action::Prep),
            alert("02-veto-trade-expiry", Action::Invalidate),
            alert("05-enter", Action::Enter),
        ];
        let roles = Roles {
            invalidation: Some(horz(1.1000)),
            break_and_close: Some(trend((10, 1.1100), (20, 1.1150))),
            retest: Some(trend((10, 1.1100), (20, 1.1150))),
            trade_expiry: Some(vert(99_000)),
            ..Roles::default()
        };

        let plan = build_trade_plan(
            "eurusd-ihs-1",
            "EUR_USD",
            &alerts,
            ConvDirection::Long,
            &geom_of(&roles),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );

        let by_id = |id: &str| plan.rules.iter().find(|r| r.rule_id == id).unwrap();

        // Long invalidation floor: a drawn line → crosses DOWN into the floor,
        // close-confirmed (OnClose).
        assert!(matches!(
            by_id("01-veto-too-low").trigger,
            Trigger::HorizontalCross {
                level,
                dir: CrossDir::Down,
                bar: BarEvent::OnClose,
            } if (level - 1.1000).abs() < 1e-9
        ));
    }

    // ===== `--trend`: the pcl-exhausted veto becomes close-confirmed =====

    /// Both roles a `too-high`/`too-low` pair can play, on ONE chart, so a test
    /// can assert the trend switch moves the fib level *without* touching the
    /// drawn cap. `direction` picks which name is which (see the CLAUDE.md
    /// table — the names swap with direction).
    ///
    /// Geometry: invalidation drawn at 1.2000, fib head 1.2000 / neckline
    /// 1.1000 ⇒ TP = 2×1.1 − 1.2 = 1.0000 and pcl-exhausted = 1.1 + 0.8×(1.0 −
    /// 1.1) = **1.0200**.
    fn trend_roles() -> Roles {
        Roles {
            invalidation: Some(horz(1.2000)),
            trade_expiry: Some(vert(99_000)),
            tp_fib: Some(trend((10, 1.1000), (20, 1.2000))),
            ..Roles::default()
        }
    }

    /// Build a short H&S plan carrying BOTH veto names, at the given trend arm.
    fn trend_plan(trend: TrendFollow, direction: ConvDirection) -> TradePlan {
        let alerts = vec![
            alert("01-veto-too-high", Action::Veto),
            alert("01-veto-too-low", Action::Veto),
            alert("02-veto-trade-expiry", Action::Invalidate),
            alert("05-enter", Action::Enter),
        ];
        build_trade_plan(
            "eurusd-trend-1",
            "EUR_USD",
            &alerts,
            direction,
            &geom_of(&trend_roles()),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            trend,
        )
    }

    fn trigger_of(plan: &TradePlan, id: &str) -> Trigger {
        plan.rules
            .iter()
            .find(|r| r.rule_id == id)
            .unwrap_or_else(|| panic!("no rule {id}"))
            .trigger
            .clone()
    }

    /// The bug this flag exists for. On a **short**, `too-low` is the
    /// pcl-exhausted fib level; by default it is a wick-through straddle, so a
    /// spike that closes back inside still aborts the trade — which is what
    /// killed the 2026-08-07 trend short (`01-veto-too-low` fired ~30 min after
    /// arming, off a wick). Under `--trend` the bar must **close** past it.
    #[test]
    fn trend_makes_the_pcl_exhausted_veto_close_confirmed() {
        let reversal = trigger_of(
            &trend_plan(TrendFollow::REVERSAL, ConvDirection::Short),
            "01-veto-too-low",
        );
        assert!(
            matches!(
                reversal,
                Trigger::PriceValueCross {
                    level,
                    dir: CrossDir::Either,
                    bar: BarEvent::Intrabar,
                } if (level - 1.0200).abs() < 1e-9
            ),
            "without --trend the pcl-exhausted level is a wick-through straddle: {reversal:?}"
        );

        let trend = trigger_of(
            &trend_plan(TrendFollow::TREND, ConvDirection::Short),
            "01-veto-too-low",
        );
        assert!(
            matches!(
                trend,
                Trigger::PriceValueCross {
                    level,
                    dir: CrossDir::Either,
                    bar: BarEvent::OnClose,
                } if (level - 1.0200).abs() < 1e-9
            ),
            "--trend must make the pcl-exhausted level close-confirmed, same level: {trend:?}"
        );
    }

    /// The switch must move the **fib** level only. The drawn invalidation cap
    /// is the operator's line and is already `OnClose`; `--trend` must not
    /// touch its level, direction, or trigger variant — otherwise a trend arm
    /// would silently re-shape the structural invalidation too.
    #[test]
    fn trend_leaves_the_drawn_invalidation_cap_untouched() {
        for direction in [ConvDirection::Short, ConvDirection::Long] {
            // The DRAWN cap is the name matching the direction: short =>
            // too-high, long => too-low.
            let drawn = match direction {
                ConvDirection::Short => "01-veto-too-high",
                ConvDirection::Long => "01-veto-too-low",
            };
            assert_eq!(
                format!(
                    "{:?}",
                    trigger_of(&trend_plan(TrendFollow::REVERSAL, direction), drawn)
                ),
                format!(
                    "{:?}",
                    trigger_of(&trend_plan(TrendFollow::TREND, direction), drawn)
                ),
                "--trend must not change the drawn invalidation cap ({direction:?} / {drawn})"
            );
        }
    }

    /// The names swap with direction, so the switch has to follow the *role*,
    /// not the literal name: on a **long** it is `too-high` that carries the
    /// pcl-exhausted fib level, and that is the one `--trend` must flip.
    #[test]
    fn on_a_long_trend_flips_too_high_not_too_low() {
        let plan = trend_plan(TrendFollow::TREND, ConvDirection::Long);
        assert!(
            matches!(
                trigger_of(&plan, "01-veto-too-high"),
                Trigger::PriceValueCross {
                    bar: BarEvent::OnClose,
                    ..
                }
            ),
            "long: the pcl-exhausted role is `too-high` and must be close-confirmed"
        );
    }

    /// Nothing else in the plan moves. `--trend` selects one trigger's
    /// `BarEvent`; if it started leaking into the enter, the expiry, or the
    /// plan-level knobs, this catches it.
    #[test]
    fn trend_changes_only_the_pcl_exhausted_rule() {
        let a = trend_plan(TrendFollow::REVERSAL, ConvDirection::Short);
        let b = trend_plan(TrendFollow::TREND, ConvDirection::Short);
        for rule in &a.rules {
            if rule.rule_id == "01-veto-too-low" {
                continue;
            }
            assert_eq!(
                format!("{:?}", rule.trigger),
                format!("{:?}", trigger_of(&b, &rule.rule_id)),
                "--trend must not touch {}",
                rule.rule_id
            );
        }
        assert_eq!(a.rules.len(), b.rules.len(), "same rule set");
    }

    /// A built plan survives the exact JSON round-trip that `--plan-out` writes
    /// and the offline `replay-candles` harness reads back. Guards the contract
    /// between tv-arm dumping the plan and the harness deserialising it: every
    /// rule, trigger, and embedded intent must reappear unchanged.
    #[test]
    fn built_plan_round_trips_through_plan_out_json() {
        let alerts = vec![
            alert("01-veto-too-high", Action::Veto),
            alert("03-prep-break-and-close", Action::Prep),
            alert("04-prep-retest", Action::Prep),
            alert("02-veto-trade-expiry", Action::Invalidate),
            alert("05-enter", Action::Enter),
        ];
        let roles = Roles {
            invalidation: Some(horz(1.2000)),
            break_and_close: Some(trend((10, 1.1900), (20, 1.1850))),
            retest: Some(trend((10, 1.1900), (20, 1.1850))),
            trade_expiry: Some(vert(99_000)),
            ..Roles::default()
        };
        let plan = build_trade_plan(
            "eurusd-roundtrip-1",
            "EUR_USD",
            &alerts,
            ConvDirection::Short,
            &geom_of(&roles),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );

        // This is exactly what `register_trade_plan` writes for `--plan-out`.
        let json = serde_json::to_string_pretty(&plan).expect("serialise plan");
        let back: TradePlan = serde_json::from_str(&json).expect("deserialise plan");

        assert_eq!(back.trade_id, plan.trade_id);
        assert_eq!(back.instrument, plan.instrument);
        assert_eq!(back.granularity, plan.granularity);
        assert_eq!(back.direction, plan.direction);
        assert_eq!(back.pip_size, plan.pip_size);
        assert_eq!(back.shadow, plan.shadow);
        assert_eq!(back.rules.len(), plan.rules.len());
        for (a, b) in plan.rules.iter().zip(back.rules.iter()) {
            assert_eq!(a.rule_id, b.rule_id);
            assert_eq!(a.fire_mode, b.fire_mode);
            assert_eq!(a.intent.action, b.intent.action);
        }
    }

    /// An M/W enter folds to the per-bar heartbeat (EveryBar), and its
    /// path-anchor vetos become price-value triggers; abort is the only
    /// OnClose one.
    #[test]
    fn builds_mw_enter_as_heartbeat_and_price_vetos() {
        // path anchors [A, B, C] = [runup_start, first_point, neckline].
        let path = Drawing {
            id: "p".into(),
            points: vec![
                Point {
                    time: 1,
                    price: 1.1000,
                },
                Point {
                    time: 2,
                    price: 1.2000,
                },
                Point {
                    time: 3,
                    price: 1.1500,
                },
            ],
            properties: Properties::default(),
        };
        let alerts = vec![
            alert("05-enter", Action::Enter),
            alert("01-veto-mw-cancel", Action::Veto),
            alert("01-veto-mw-abort", Action::Veto),
            alert("01-veto-mw-overshoot", Action::Veto),
        ];
        let roles = Roles {
            mw_path: Some(path),
            ..Roles::default()
        };

        let plan = build_trade_plan(
            "eurusd-mw-1",
            "EUR_USD",
            &alerts,
            ConvDirection::Short,
            &geom_of(&roles),
            Granularity::H1,
            true,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );
        let by_id = |id: &str| plan.rules.iter().find(|r| r.rule_id == id).unwrap();

        let enter = by_id("05-enter");
        assert_eq!(enter.trigger, Trigger::MwEveryBar);
        assert_eq!(enter.fire_mode, FireMode::EveryBar);

        assert!(matches!(
            by_id("01-veto-mw-cancel").trigger,
            Trigger::PriceValueCross {
                bar: BarEvent::Intrabar,
                ..
            }
        ));
        assert!(matches!(
            by_id("01-veto-mw-abort").trigger,
            Trigger::PriceValueCross {
                bar: BarEvent::OnClose,
                ..
            }
        ));
        assert!(matches!(
            by_id("01-veto-mw-overshoot").trigger,
            Trigger::PriceValueCross {
                bar: BarEvent::Intrabar,
                ..
            }
        ));
    }

    /// An alert whose supporting role isn't on the chart is skipped (same
    /// `Ok(None)` semantics as `build_alert_spec`).
    #[test]
    fn missing_role_skips_the_rule() {
        let alerts = vec![alert("04-prep-retest", Action::Prep)];
        // No retest trendline in roles → no rule.
        let plan = build_trade_plan(
            "t",
            "EUR_USD",
            &alerts,
            ConvDirection::Short,
            &PlanGeometry::from_roles(&Roles::default()),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );
        assert!(plan.rules.is_empty());
    }

    /// `shadow=true` is carried through onto the built plan, so a
    /// `--register-plan --shadow` arm produces an observe-only plan.
    #[test]
    fn shadow_flag_carried_onto_plan() {
        let alerts = vec![alert("05-enter", Action::Enter)];
        let plan = build_trade_plan(
            "t",
            "EUR_USD",
            &alerts,
            ConvDirection::Short,
            &PlanGeometry::from_roles(&Roles::default()),
            Granularity::H1,
            true,
            true,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );
        assert!(plan.shadow, "shadow=true must reach the built plan");
    }

    #[test]
    fn retest_atr_step_carried_onto_plan() {
        let alerts = vec![alert("05-enter", Action::Enter)];
        // A custom step threads through to the signed plan field.
        let custom = build_trade_plan(
            "t",
            "EUR_USD",
            &alerts,
            ConvDirection::Short,
            &PlanGeometry::from_roles(&Roles::default()),
            Granularity::H1,
            false,
            false,
            None,
            0.2,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );
        assert!(
            (custom.retest_atr_step - 0.2).abs() < 1e-9,
            "--retest-atr-step value must reach the built plan, got {}",
            custom.retest_atr_step
        );
        // The pipeline passes the default const when the flag is absent.
        let defaulted = build_trade_plan(
            "t",
            "EUR_USD",
            &alerts,
            ConvDirection::Short,
            &PlanGeometry::from_roles(&Roles::default()),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );
        assert!(
            (defaulted.retest_atr_step - 0.075).abs() < 1e-9,
            "default step is 0.075, got {}",
            defaulted.retest_atr_step
        );
    }

    #[test]
    fn cross_buffer_pct_carried_onto_plan() {
        let alerts = vec![alert("05-enter", Action::Enter)];
        // A custom buffer (e.g. `--cross-buffer-pct 0`) threads to the plan field.
        let custom = build_trade_plan(
            "t",
            "EUR_USD",
            &alerts,
            ConvDirection::Short,
            &PlanGeometry::from_roles(&Roles::default()),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            0.0,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );
        assert!(
            custom.cross_buffer_pct.abs() < 1e-9,
            "--cross-buffer-pct 0 must reach the built plan, got {}",
            custom.cross_buffer_pct
        );
        // The pipeline passes the default const when the flag is absent.
        let defaulted = build_trade_plan(
            "t",
            "EUR_USD",
            &alerts,
            ConvDirection::Short,
            &PlanGeometry::from_roles(&Roles::default()),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );
        assert!(
            (defaulted.cross_buffer_pct - trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT)
                .abs()
                < 1e-9,
            "default buffer is {}, got {}",
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            defaulted.cross_buffer_pct
        );
    }

    #[test]
    fn cross_buffer_atr_carried_onto_plan() {
        let alerts = vec![alert("05-enter", Action::Enter)];
        // A custom ATR-fraction buffer (`--cross-buffer-atr 0.15`) reaches the plan.
        let custom = build_trade_plan(
            "t",
            "EUR_USD",
            &alerts,
            ConvDirection::Short,
            &PlanGeometry::from_roles(&Roles::default()),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            0.15,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );
        assert!(
            (custom.cross_buffer_atr - 0.15).abs() < 1e-9,
            "--cross-buffer-atr must reach the built plan, got {}",
            custom.cross_buffer_atr
        );
        // Default when the flag is absent is DEFAULT_CROSS_BUFFER_ATR (0.0 = off).
        let defaulted = build_trade_plan(
            "t",
            "EUR_USD",
            &alerts,
            ConvDirection::Short,
            &PlanGeometry::from_roles(&Roles::default()),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );
        assert!(
            defaulted.cross_buffer_atr.abs() < 1e-9,
            "default cross_buffer_atr is 0.0 (off), got {}",
            defaulted.cross_buffer_atr
        );
    }

    // ===== append_control_rules =====

    use trade_control_cli::{NewsSpec, PauseSpec, build_news_from_spec, build_pause_from_spec};
    use trade_control_core::intent::{Action as CoreAction, BrokerKind};

    fn pause_spec(trade_id: &str, start: &str, end: &str) -> PauseSpec {
        PauseSpec {
            trade_id: trade_id.into(),
            blackout_id: None,
            instrument: "EUR_USD".into(),
            account: "demo".into(),
            broker: BrokerKind::Oanda,
            start_time: ts(start),
            end_time: ts(end),
            reason: None,
        }
    }

    fn news_spec(trade_id: &str, start: &str, end: &str) -> NewsSpec {
        NewsSpec {
            trade_id: trade_id.into(),
            news_id: None,
            instrument: "EUR_USD".into(),
            account: "demo".into(),
            broker: BrokerKind::Oanda,
            start_time: ts(start),
            end_time: ts(end),
            reason: None,
        }
    }

    /// A plan with one pause window and one news window (both now sourced from
    /// the calendar, arriving as built bundles) gains a `TimeReached` rule per
    /// window edge, each carrying the matching control action at the right epoch.
    #[test]
    fn control_rules_appended_from_pause_and_news_bundles() {
        let now = ts("2026-06-15T00:00:00Z");
        let pause = build_pause_from_spec(
            pause_spec("t", "2026-06-16T10:00:00Z", "2026-06-16T11:00:00Z"),
            now,
        )
        .unwrap();
        let news = build_news_from_spec(
            news_spec("t", "2026-06-16T12:00:00Z", "2026-06-16T13:00:00Z"),
            now,
        )
        .unwrap();

        let mut plan = build_trade_plan(
            "t",
            "EUR_USD",
            &[alert("05-enter", Action::Enter)],
            ConvDirection::Short,
            &PlanGeometry::from_roles(&Roles::default()),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            chrono::Utc::now(),
            None,
            None, // pullback_arm
            None, // screenshot_url
            TrendFollow::REVERSAL,
        );
        assert_eq!(plan.rules.len(), 1, "just the enter before appending");

        append_control_rules(&mut plan, &[&pause], &[&news]);

        // 1 enter + (pause-start, pause-resume) + (news-start, news-end) = 5.
        assert_eq!(plan.rules.len(), 5);

        let by_action = |a: CoreAction| {
            plan.rules
                .iter()
                .filter(|r| r.intent.action == a)
                .collect::<Vec<_>>()
        };
        assert_eq!(by_action(CoreAction::Pause).len(), 1);
        assert_eq!(by_action(CoreAction::Resume).len(), 1);
        assert_eq!(by_action(CoreAction::NewsStart).len(), 1);
        assert_eq!(by_action(CoreAction::NewsEnd).len(), 1);

        // The pause anchors its start/end epochs to the window edges.
        let pause_start = by_action(CoreAction::Pause)
            .into_iter()
            .find(|r| {
                matches!(r.trigger, Trigger::TimeReached { at_epoch }
                    if at_epoch == ts("2026-06-16T10:00:00Z").timestamp())
            })
            .expect("pause-start at its start epoch");
        assert_eq!(pause_start.fire_mode, FireMode::Once);

        // The news-end anchors to the news window's end.
        assert!(
            by_action(CoreAction::NewsEnd).iter().any(|r| {
                matches!(r.trigger, Trigger::TimeReached { at_epoch }
                    if at_epoch == ts("2026-06-16T13:00:00Z").timestamp())
            }),
            "news-end at the news window end"
        );
    }
}
