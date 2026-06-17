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

use trade_control_cli::BuiltAlert;
use trade_control_conventions::{AlertBasename, Direction as ConvDirection};
use trade_control_core::broker::Granularity;
use trade_control_core::intent::Direction;
use trade_control_core::trade_plan::{
    BarEvent, ConditionRule, CrossDir, FireMode, LinePoint, TradePlan, Trigger,
};

use crate::geometry::pcl_exhausted_price_from_fib;
use crate::mw_geometry::{abort_level, cancel_level, overshoot_level};
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
// Eight parameters: each is a distinct chart-derived primitive (id, instrument,
// alerts, direction, roles, granularity, is_mw, shadow) threaded once from the
// single pipeline call site. Grouping them into a struct would just move the
// same fields elsewhere without clarifying anything.
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
) -> TradePlan {
    let rules = alerts
        .iter()
        .filter_map(|alert| build_rule(alert, direction, roles, is_mw))
        .collect();

    TradePlan {
        trade_id: trade_id.to_string(),
        instrument: instrument.to_string(),
        direction: to_core_direction(direction),
        granularity,
        pip_size: pip_size_of(alerts),
        rules,
        shadow,
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
    is_mw: bool,
) -> Option<ConditionRule> {
    let basename = AlertBasename::parse(&alert.basename)?;
    let trigger = trigger_for(&basename, direction, roles, is_mw)?;
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
    is_mw: bool,
) -> Option<Trigger> {
    match basename {
        AlertBasename::VetoTooHigh | AlertBasename::VetoTooLow => {
            invalidation_or_pcl_trigger(basename, direction, roles)
        }
        // Trade-expiry / prep-expiry / pause / news are vertical-line time
        // triggers. The veto fires when wall-clock reaches the line.
        AlertBasename::VetoTradeExpiry => time_trigger(roles.trade_expiry.as_ref()),
        AlertBasename::PrepExpire(step) => time_trigger(
            roles
                .prep_expiries
                .iter()
                .find(|(s, _)| s == step)
                .map(|(_, d)| d),
        ),
        AlertBasename::PauseStart(_) => time_trigger(roles.blackout_pairs.first().map(|(s, _)| s)),
        AlertBasename::PauseResume(_) => time_trigger(roles.blackout_pairs.first().map(|(_, e)| e)),
        AlertBasename::NewsStart(_) => time_trigger(roles.news_pairs.first().map(|(s, _)| s)),
        AlertBasename::NewsEnd(_) => time_trigger(roles.news_pairs.first().map(|(_, e)| e)),
        // Break-and-close: neckline trendline, closes through it. Short closes
        // down, long closes up — same as the TV `CrossDown`/`CrossUp`.
        AlertBasename::PrepBreakAndClose => trendline_trigger(
            roles.break_and_close.as_ref(),
            close_dir(direction),
            BarEvent::OnClose,
        ),
        // Retest: opposite cross of the neckline trendline, intrabar.
        AlertBasename::PrepRetest => trendline_trigger(
            roles.retest.as_ref(),
            retest_dir(direction),
            BarEvent::Intrabar,
        ),
        // Enter: H&S binds to the direction's candle pattern; M/W to the
        // per-bar geometry heartbeat.
        AlertBasename::Enter => Some(if is_mw {
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
        // Drawing-bound invalidation: short crosses up into the cap, long
        // crosses down into the floor. Intrabar, fire-once.
        let d = roles.invalidation.as_ref()?;
        Some(Trigger::HorizontalCross {
            level: horizontal_level(d)?,
            dir: match direction {
                ConvDirection::Short => CrossDir::Up,
                ConvDirection::Long => CrossDir::Down,
            },
            bar: BarEvent::Intrabar,
        })
    } else {
        // Opposite-name veto = pcl-exhausted, a computed price value.
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
    let (level, bar) = match which {
        MwVeto::Cancel => (cancel_level(first_point, neckline), BarEvent::Intrabar),
        // Abort is the only M/W veto that's a candle *close* back through the
        // neckline → OnClose.
        MwVeto::Abort => (abort_level(neckline), BarEvent::OnClose),
        MwVeto::Overshoot => (overshoot_level(first_point, neckline), BarEvent::Intrabar),
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
fn trendline_trigger(drawing: Option<&Drawing>, dir: CrossDir, bar: BarEvent) -> Option<Trigger> {
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
        );

        assert!(!plan.shadow, "default build is live, not shadow");
        assert_eq!(plan.trade_id, "eurusd-hs-1");
        assert_eq!(plan.granularity, Granularity::H1);
        assert_eq!(plan.direction, Direction::Short);
        assert_eq!(plan.pip_size, 0.0001);
        assert_eq!(plan.rules.len(), 5);

        let by_id = |id: &str| plan.rules.iter().find(|r| r.rule_id == id).unwrap();

        // Invalidation: short crosses UP into the cap, intrabar, fire-once.
        assert!(matches!(
            by_id("01-veto-too-high").trigger,
            Trigger::HorizontalCross {
                level,
                dir: CrossDir::Up,
                bar: BarEvent::Intrabar,
            } if (level - 1.2000).abs() < 1e-9
        ));
        // Break-and-close: short closes DOWN through the neckline, OnClose.
        assert!(matches!(
            by_id("03-prep-break-and-close").trigger,
            Trigger::TrendlineCross {
                dir: CrossDir::Down,
                bar: BarEvent::OnClose,
                extend_forward: true,
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
        );
        assert!(plan.shadow, "shadow=true must reach the built plan");
    }
}
