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

use trade_control_cli::{BuiltAlert, BuiltCalendarBundle, BuiltNews, BuiltPause};
use trade_control_conventions::{AlertBasename, Direction as ConvDirection};
use trade_control_core::broker::Granularity;
use trade_control_core::intent::{Direction, Intent};
use trade_control_core::trade_plan::{
    BarEvent, ConditionRule, CrossDir, FireMode, LinePoint, TradePlan, Trigger,
};

use crate::geometry::pcl_exhausted_price_from_fib;
use crate::mw_geometry::{abort_level, cancel_level, highest_shoulder, overshoot_level};
use crate::roles::Roles;
use trading_view::drawings::Drawing;

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
//
// Ten parameters: each is a distinct chart-derived primitive (id, instrument,
// alerts, direction, roles, granularity, is_mw, shadow, replay_start,
// retest_atr_step) threaded once from the single pipeline call site. Grouping
// them into a struct would just move the same fields elsewhere without
// clarifying anything.
#[allow(clippy::too_many_arguments)]
pub fn build_trade_plan(
    trade_id: &str,
    instrument: &str,
    alerts: &[BuiltAlert],
    direction: ConvDirection,
    roles: &Roles,
    granularity: Granularity,
    is_mw: bool,
    shadow: bool,
    replay_start: Option<i64>,
    retest_atr_step: f64,
) -> TradePlan {
    let rules = alerts
        .iter()
        .filter_map(|alert| build_rule(alert, direction, roles, granularity, is_mw))
        .collect();

    TradePlan {
        trade_id: trade_id.to_string(),
        instrument: instrument.to_string(),
        direction: to_core_direction(direction),
        granularity,
        pip_size: pip_size_of(alerts),
        rules,
        shadow,
        cross_buffer_pct: trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
        retest_atr_step,
        replay_start,
    }
}

/// Append the pause/news/calendar **control bars** to a built plan as
/// `TimeReached` rules — one per bundle alert, carrying that alert's embedded
/// intent verbatim and firing at the bundle's window edge (start for
/// pause-start / news-start, end for pause-resume / news-end).
///
/// This is what makes `--register-plan` open/close the same blackout + news
/// windows the `--create-alerts` path POSTs as TradingView alerts. The chart-
/// drawn pairs arrive as [`BuiltPause`]/[`BuiltNews`] bundles; the auto-fetched
/// forex-factory events arrive as [`BuiltCalendarBundle`]s (each holding one
/// pause + one news). All three feed the same per-alert conversion.
///
/// `build_trade_plan`'s `trigger_for` deliberately does **not** handle these
/// basenames anymore: it only ever saw `roles.*_pairs.first()` (one pair) and
/// the control alerts were never in `built_trade.alerts` to begin with — so the
/// rules came from here, where every window (and every calendar event) is
/// represented.
pub fn append_control_rules(
    plan: &mut TradePlan,
    pause_bundles: &[&BuiltPause],
    news_bundles: &[&BuiltNews],
    calendar_bundles: &[BuiltCalendarBundle],
) {
    for b in pause_bundles {
        push_window_rules(plan, &b.alerts, b.start_time, b.end_time);
    }
    for b in news_bundles {
        push_window_rules(plan, &b.alerts, b.start_time, b.end_time);
    }
    for cb in calendar_bundles {
        push_window_rules(
            plan,
            &cb.pause.alerts,
            cb.pause.start_time,
            cb.pause.end_time,
        );
        push_window_rules(plan, &cb.news.alerts, cb.news.start_time, cb.news.end_time);
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
    roles: &Roles,
    granularity: Granularity,
    is_mw: bool,
) -> Option<ConditionRule> {
    let basename = AlertBasename::parse(&alert.basename)?;
    let trigger = trigger_for(&basename, direction, roles, granularity, is_mw)?;
    let fire_mode = fire_mode_for(&trigger);
    Some(ConditionRule {
        rule_id: alert.basename.clone(),
        trigger,
        fire_mode,
        intent: alert.intent.clone(),
    })
}

/// The 1:1 port of `build_alert_spec`'s basename → condition dispatch,
/// re-expressed as a [`Trigger`]. Returns `None` for a missing role (same
/// skip semantics) or a basename with no server-side trigger.
fn trigger_for(
    basename: &AlertBasename,
    direction: ConvDirection,
    roles: &Roles,
    granularity: Granularity,
    is_mw: bool,
) -> Option<Trigger> {
    match basename {
        AlertBasename::VetoTooHigh | AlertBasename::VetoTooLow => {
            invalidation_or_pcl_trigger(basename, direction, roles)
        }
        // Trade-expiry / prep-expiry are vertical-line time triggers. The veto
        // fires when wall-clock reaches the line.
        AlertBasename::VetoTradeExpiry => time_trigger(roles.trade_expiry.as_ref()),
        AlertBasename::PrepExpire(step) => time_trigger(
            roles
                .prep_expiries
                .iter()
                .find(|(s, _)| s == step)
                .map(|(_, d)| d),
        ),
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
            roles.break_and_close.as_ref(),
            close_dir(direction),
            BarEvent::OnClose,
            granularity,
        ),
        // Retest: opposite cross of the neckline trendline, intrabar.
        AlertBasename::PrepRetest => trendline_trigger(
            roles.retest.as_ref(),
            retest_dir(direction),
            BarEvent::Intrabar,
            granularity,
        ),
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
        AlertBasename::VetoMwCancel => mw_price_trigger(roles, MwVeto::Cancel),
        AlertBasename::VetoMwAbort => mw_price_trigger(roles, MwVeto::Abort),
        AlertBasename::VetoMwOvershoot => mw_price_trigger(roles, MwVeto::Overshoot),
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
    roles: &Roles,
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
        let d = roles.invalidation.as_ref()?;
        let dir = match direction {
            ConvDirection::Short => CrossDir::Up,  // close above the cap
            ConvDirection::Long => CrossDir::Down, // close below the floor
        };
        Some(Trigger::HorizontalCross {
            level: horizontal_level(d)?,
            dir,
            bar: BarEvent::OnClose,
        })
    } else {
        // Opposite-name veto = pcl-exhausted, a computed **fib** level ("the
        // power of the setup has been consumed"). A fib level is a
        // **wick-through** (`Intrabar`, `Either`): any straddle aborts — if the
        // move ran ~80% to TP without us, a wick alone is reason enough.
        let fib = roles.tp_fib.as_ref()?;
        Some(Trigger::PriceValueCross {
            level: pcl_exhausted_price_from_fib(&fib.prices(), direction),
            dir: CrossDir::Either,
            bar: BarEvent::Intrabar,
        })
    }
}

/// Build an M/W cancel / abort / overshoot price-value trigger from the path
/// anchors. Verbatim port of `alert_spec::mw_price_veto`.
fn mw_price_trigger(roles: &Roles, which: MwVeto) -> Option<Trigger> {
    let path = roles.mw_path.as_ref()?;
    let first_point = path.points.get(1)?.price;
    let neckline = path.points.get(2)?.price;
    // 4-point path: anchor the cancel / overshoot levels to the **higher** of
    // the two drawn shoulders, so a drawn right shoulder above the left widens
    // the 1.3 cancel ceiling and pushes the overshoot level out to match the
    // real geometry. The abort (neckline close) is shoulder-independent.
    let right_shoulder = path.points.get(3).map(|p| p.price);
    let shoulder = highest_shoulder(first_point, neckline, right_shoulder);
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

/// A vertical-line time trigger from a drawing's anchor, or `None` if the
/// drawing is absent.
fn time_trigger(drawing: Option<&Drawing>) -> Option<Trigger> {
    let d = drawing?;
    Some(Trigger::TimeReached {
        at_epoch: d.points.first()?.time,
    })
}

/// A trendline cross trigger from a two-anchor drawing. Necklines are
/// extended forward so a cross past the right anchor still fires (the engine
/// analogue of the TV `extend_forward` flag — see the README trendline note).
fn trendline_trigger(
    drawing: Option<&Drawing>,
    dir: CrossDir,
    bar: BarEvent,
    granularity: Granularity,
) -> Option<Trigger> {
    let d = drawing?;
    let a = d.points.first()?;
    let b = d.points.get(1)?;
    Some(Trigger::TrendlineCross {
        a: LinePoint {
            at_epoch: a.time,
            price: a.price,
        },
        b: LinePoint {
            at_epoch: b.time,
            price: b.price,
        },
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

/// The horizontal line's price level — the first (only) anchor's price.
fn horizontal_level(d: &Drawing) -> Option<f64> {
    Some(d.points.first()?.price)
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
            &roles,
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
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
            &roles,
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
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
            &roles,
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
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
            &roles,
            Granularity::H1,
            true,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
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
            &Roles::default(),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
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
            &Roles::default(),
            Granularity::H1,
            true,
            true,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
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
            &Roles::default(),
            Granularity::H1,
            false,
            false,
            None,
            0.2,
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
            &Roles::default(),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
        );
        assert!(
            (defaulted.retest_atr_step - 0.075).abs() < 1e-9,
            "default step is 0.075, got {}",
            defaulted.retest_atr_step
        );
    }

    // ===== append_control_rules =====

    use trade_control_cli::{
        BuiltCalendarBundle, NewsSpec, PauseSpec, build_news_from_spec, build_pause_from_spec,
    };
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

    /// A plan with one chart-drawn pause pair, one news pair, and a calendar
    /// event (its own pause + news) gains a `TimeReached` rule per window edge,
    /// each carrying the matching control action at the right epoch.
    #[test]
    fn control_rules_appended_from_pause_news_and_calendar_bundles() {
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
        // Calendar event: a separate pause + news window.
        let cal = BuiltCalendarBundle {
            event_slug: "usd-cpi-1".into(),
            pause: build_pause_from_spec(
                pause_spec("t", "2026-06-17T14:00:00Z", "2026-06-17T14:30:00Z"),
                now,
            )
            .unwrap(),
            news: build_news_from_spec(
                news_spec("t", "2026-06-17T14:30:00Z", "2026-06-17T15:00:00Z"),
                now,
            )
            .unwrap(),
        };

        let mut plan = build_trade_plan(
            "t",
            "EUR_USD",
            &[alert("05-enter", Action::Enter)],
            ConvDirection::Short,
            &Roles::default(),
            Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
        );
        assert_eq!(plan.rules.len(), 1, "just the enter before appending");

        append_control_rules(&mut plan, &[&pause], &[&news], &[cal]);

        // 1 enter + (pause-start, pause-resume) + (news-start, news-end)
        // + calendar (pause start/resume + news start/end) = 1 + 2 + 2 + 4 = 9.
        assert_eq!(plan.rules.len(), 9);

        let by_action = |a: CoreAction| {
            plan.rules
                .iter()
                .filter(|r| r.intent.action == a)
                .collect::<Vec<_>>()
        };
        // Two pause windows (chart + calendar) → two Pause + two Resume.
        assert_eq!(by_action(CoreAction::Pause).len(), 2);
        assert_eq!(by_action(CoreAction::Resume).len(), 2);
        // Two news windows → two NewsStart + two NewsEnd.
        assert_eq!(by_action(CoreAction::NewsStart).len(), 2);
        assert_eq!(by_action(CoreAction::NewsEnd).len(), 2);

        // The chart pause anchors its start/end epochs to the window edges.
        let pause_start = by_action(CoreAction::Pause)
            .into_iter()
            .find(|r| {
                matches!(r.trigger, Trigger::TimeReached { at_epoch }
                    if at_epoch == ts("2026-06-16T10:00:00Z").timestamp())
            })
            .expect("chart pause-start at its start epoch");
        assert_eq!(pause_start.fire_mode, FireMode::Once);

        // The calendar news-end anchors to the calendar window's end.
        assert!(
            by_action(CoreAction::NewsEnd).iter().any(|r| {
                matches!(r.trigger, Trigger::TimeReached { at_epoch }
                    if at_epoch == ts("2026-06-17T15:00:00Z").timestamp())
            }),
            "calendar news-end at the calendar window end"
        );
    }
}
