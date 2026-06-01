//! Typed dispatcher from a manifest entry to the JSON payload the
//! create-alerts JS template consumes.
//!
//! Port of `tv_arm_hs.py`'s `build_alert_spec()` (~lines 819–1081).
//! The dispatch axis is the alert's basename (parsed via
//! [`AlertBasename`]); the role data comes from [`Roles`] (slice 3)
//! and the geometry helpers in [`crate::geometry`].
//!
//! The Python returns `None` to mean "skip this entry"; we return
//! `Option<AlertPayload>` with the same semantics. The downstream
//! orchestrator (slice 5) stamps `name`, `message`, and the
//! `<trade_id>-` prefix on `tv_name` after this — we leave those
//! fields untouched here so this module stays a pure data dispatch.

use chrono::DateTime;
use color_eyre::eyre::{Result, eyre};
use serde::Serialize;
use trade_control_conventions::{
    AlertBasename, Direction, PINE_INDICATOR_NAME, entry_plot_for, reversal_close_plot_for,
};

use crate::geometry::pcl_exhausted_price_from_fib;
use crate::roles::Roles;
use trading_view::drawings::Drawing;

/// Calendar-window context for an alert that's bound to a synthetic
/// vertical line (the operator never drew it on the chart). Carried
/// instead of a `(Drawing, Drawing)` pair.
#[derive(Debug, Clone, PartialEq)]
pub struct CalendarWindow {
    /// ISO-8601 timestamp of the window's left edge.
    pub start_iso: String,
    /// ISO-8601 timestamp of the window's right edge.
    pub end_iso: String,
}

/// Optional context fed alongside the manifest entry. Each pause /
/// news / calendar bundle's manifest gets dispatched with the matching
/// context populated; everything else is `None`.
#[derive(Debug, Default, Clone)]
pub struct DispatchContext<'a> {
    /// Operator-drawn blackout pair for `01-pause-*` / `02-resume-*`.
    pub blackout_pair: Option<(&'a Drawing, &'a Drawing)>,
    /// Operator-drawn news pair for `01-news-start-*` / `02-news-end-*`.
    pub news_pair: Option<(&'a Drawing, &'a Drawing)>,
    /// Calendar-derived window for `cal-*` basenames.
    pub calendar_window: Option<CalendarWindow>,
}

/// One alert payload, ready to be serialized into the JSON array the
/// create-alerts JS reads.
///
/// All variants share a `tv_name`, `frequency`, and `auto_deactivate`
/// tail; the discriminator (`kind` in the wire JSON) tells the JS
/// which condition branch to take.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AlertPayload {
    /// Drawing-bound (the alert tracks a real chart shape via
    /// `drawing_id`).
    Drawing {
        drawing_id: String,
        tool: Tool,
        condition_type: ConditionType,
        frequency: Frequency,
        auto_deactivate: bool,
        tv_name: String,
    },
    /// Numeric-price-bound (the alert tracks a computed value with no
    /// drawing on the chart — currently pcl-exhausted veto only).
    PriceValue {
        value: f64,
        condition_type: ConditionType,
        frequency: Frequency,
        auto_deactivate: bool,
        tv_name: String,
    },
    /// Synthetic vertical-line at a specific epoch time (calendar
    /// bars). The JS template builds the `LineToolVertLine` shape
    /// from `base_time_epoch` without a `drawing_id` lookup.
    VertLineAt {
        base_time_epoch: i64,
        tool: Tool,
        condition_type: ConditionType,
        frequency: Frequency,
        auto_deactivate: bool,
        tv_name: String,
    },
    /// Pine-indicator-bound (the alert listens to an
    /// `alertcondition()` plot — used for `05-enter`,
    /// `06-close-on-reversal`, `07-close-on-sr-reversal`).
    PineAlertcondition {
        indicator_name: String,
        alert_cond_id: String,
        frequency: Frequency,
        auto_deactivate: bool,
        tv_name: String,
    },
}

/// TradingView drawing-tool enum, serialised verbatim into the
/// payload so the JS template can dispatch on it. Variant names
/// mirror TV's internal class names; the shared `LineTool` prefix is
/// part of the wire format.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum Tool {
    LineToolHorzLine,
    LineToolTrendLine,
    LineToolVertLine,
}

/// TradingView condition-type enum (the `cross_up` / `cross_down` /
/// `cross` value the alert engine accepts).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConditionType {
    Cross,
    CrossUp,
    CrossDown,
}

/// TradingView alert-frequency enum.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Frequency {
    OnFirstFire,
    OnBarClose,
}

/// Build one alert payload from a manifest entry. Returns `Ok(None)`
/// when the entry is recognized but its supporting role isn't on the
/// chart (the orchestrator logs and skips); `Ok(Some(_))` on success;
/// `Err` only for parse failures of supplied data (e.g. unparseable
/// ISO timestamp).
pub fn build_alert_spec(
    file: &str,
    direction: Direction,
    roles: &Roles,
    ctx: &DispatchContext<'_>,
) -> Result<Option<AlertPayload>> {
    let base = file.strip_suffix(".yaml").unwrap_or(file);
    let basename = match AlertBasename::parse(base) {
        Some(b) => b,
        None => return Ok(None),
    };
    let tv_name = role_slug(base);

    // Calendar-window short-circuit: synthetic vertical line at the
    // window edge, no chart drawing lookup.
    if let Some(window) = &ctx.calendar_window {
        return calendar_payload(&basename, window, tv_name);
    }

    Ok(match basename {
        AlertBasename::VetoTooHigh | AlertBasename::VetoTooLow => {
            invalidation_or_pcl(&basename, direction, roles, tv_name)
        }
        AlertBasename::VetoTradeExpiry => {
            roles.trade_expiry.as_ref().map(|d| AlertPayload::Drawing {
                drawing_id: d.id.clone(),
                tool: Tool::LineToolVertLine,
                condition_type: ConditionType::Cross,
                frequency: Frequency::OnFirstFire,
                auto_deactivate: false,
                tv_name,
            })
        }
        AlertBasename::PrepBreakAndClose => {
            roles
                .break_and_close
                .as_ref()
                .map(|d| AlertPayload::Drawing {
                    drawing_id: d.id.clone(),
                    tool: Tool::LineToolTrendLine,
                    condition_type: match direction {
                        Direction::Short => ConditionType::CrossDown,
                        Direction::Long => ConditionType::CrossUp,
                    },
                    frequency: Frequency::OnBarClose,
                    auto_deactivate: false,
                    tv_name,
                })
        }
        AlertBasename::PrepRetest => roles.retest.as_ref().map(|d| AlertPayload::Drawing {
            drawing_id: d.id.clone(),
            tool: Tool::LineToolTrendLine,
            condition_type: match direction {
                Direction::Short => ConditionType::CrossUp,
                Direction::Long => ConditionType::CrossDown,
            },
            frequency: Frequency::OnFirstFire,
            auto_deactivate: false,
            tv_name,
        }),
        AlertBasename::Enter => Some(pine_payload(
            entry_plot_for(direction),
            Frequency::OnBarClose,
            tv_name,
        )),
        AlertBasename::CloseOnReversal => Some(pine_payload(
            reversal_close_plot_for(direction),
            Frequency::OnBarClose,
            tv_name,
        )),
        AlertBasename::PauseStart(_) => ctx.blackout_pair.map(|(start, _)| AlertPayload::Drawing {
            drawing_id: start.id.clone(),
            tool: Tool::LineToolVertLine,
            condition_type: ConditionType::Cross,
            frequency: Frequency::OnFirstFire,
            auto_deactivate: false,
            tv_name,
        }),
        AlertBasename::PauseResume(_) => ctx.blackout_pair.map(|(_, end)| AlertPayload::Drawing {
            drawing_id: end.id.clone(),
            tool: Tool::LineToolVertLine,
            condition_type: ConditionType::Cross,
            frequency: Frequency::OnFirstFire,
            auto_deactivate: false,
            tv_name,
        }),
        AlertBasename::NewsStart(_) => ctx.news_pair.map(|(start, _)| AlertPayload::Drawing {
            drawing_id: start.id.clone(),
            tool: Tool::LineToolVertLine,
            condition_type: ConditionType::Cross,
            frequency: Frequency::OnFirstFire,
            auto_deactivate: false,
            tv_name,
        }),
        AlertBasename::NewsEnd(_) => ctx.news_pair.map(|(_, end)| AlertPayload::Drawing {
            drawing_id: end.id.clone(),
            tool: Tool::LineToolVertLine,
            condition_type: ConditionType::Cross,
            frequency: Frequency::OnFirstFire,
            auto_deactivate: false,
            tv_name,
        }),
    })
}

/// Translate `01-veto-too-high.yaml` / `01-veto-too-low.yaml` into
/// either the invalidation drawing-bound alert (when the basename
/// matches the trade direction's natural invalidation label) or the
/// pcl-exhausted price-value alert (the other one).
fn invalidation_or_pcl(
    basename: &AlertBasename,
    direction: Direction,
    roles: &Roles,
    tv_name: String,
) -> Option<AlertPayload> {
    let basename_dir = match basename {
        AlertBasename::VetoTooHigh => Direction::Short,
        AlertBasename::VetoTooLow => Direction::Long,
        _ => return None,
    };
    if basename_dir == direction {
        // Drawing-bound invalidation veto.
        let d = roles.invalidation.as_ref()?;
        let condition_type = match direction {
            Direction::Short => ConditionType::CrossUp,
            Direction::Long => ConditionType::CrossDown,
        };
        Some(AlertPayload::Drawing {
            drawing_id: d.id.clone(),
            tool: Tool::LineToolHorzLine,
            condition_type,
            frequency: Frequency::OnFirstFire,
            auto_deactivate: false,
            tv_name,
        })
    } else {
        // Opposite-name veto = pcl-exhausted, price-value bound.
        let fib = roles.tp_fib.as_ref()?;
        let value = pcl_exhausted_price_from_fib(&fib.prices(), direction);
        Some(AlertPayload::PriceValue {
            value,
            condition_type: ConditionType::Cross,
            frequency: Frequency::OnFirstFire,
            auto_deactivate: false,
            tv_name,
        })
    }
}

/// Build the synthetic-vertical-line payload for a calendar-derived
/// alert. Pause/news-start → left edge; resume/news-end → right edge.
fn calendar_payload(
    basename: &AlertBasename,
    window: &CalendarWindow,
    tv_name: String,
) -> Result<Option<AlertPayload>> {
    let edge_iso = match basename {
        AlertBasename::PauseStart(_) | AlertBasename::NewsStart(_) => &window.start_iso,
        AlertBasename::PauseResume(_) | AlertBasename::NewsEnd(_) => &window.end_iso,
        _ => return Ok(None),
    };
    let edge_epoch = DateTime::parse_from_rfc3339(edge_iso)
        .map_err(|e| eyre!("invalid ISO timestamp {edge_iso:?}: {e}"))?
        .timestamp();
    Ok(Some(AlertPayload::VertLineAt {
        base_time_epoch: edge_epoch,
        tool: Tool::LineToolVertLine,
        condition_type: ConditionType::Cross,
        frequency: Frequency::OnFirstFire,
        auto_deactivate: false,
        tv_name,
    }))
}

fn pine_payload(plot_id: &str, frequency: Frequency, tv_name: String) -> AlertPayload {
    AlertPayload::PineAlertcondition {
        indicator_name: PINE_INDICATOR_NAME.to_string(),
        alert_cond_id: plot_id.to_string(),
        frequency,
        auto_deactivate: false,
        tv_name,
    }
}

/// Strip the `NN-` prefix from a basename. Mirrors the Python
/// `base.split("-", 1)[1] if "-" in base else base` line — used as
/// the raw TV alert title slug before the orchestrator stamps the
/// `<trade_id>-` prefix.
fn role_slug(base: &str) -> String {
    match base.split_once('-') {
        Some((_, rest)) => rest.to_string(),
        None => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trading_view::drawings::{Point, Properties};

    fn drawing(id: &str, label: &str, points: Vec<(i64, f64)>) -> Drawing {
        Drawing {
            id: id.to_string(),
            points: points
                .into_iter()
                .map(|(t, p)| Point { time: t, price: p })
                .collect(),
            properties: Properties {
                text: Some(label.to_string()),
            },
        }
    }

    fn short_roles_full() -> Roles {
        Roles {
            invalidation: Some(drawing("inv", "too-high", vec![(100, 1.25)])),
            invalidation_label: Some("too-high".into()),
            break_and_close: Some(drawing("neck", "neckline", vec![(50, 1.10), (200, 1.10)])),
            retest: Some(drawing("re", "retest", vec![(50, 1.10), (200, 1.10)])),
            tp_fib: Some(drawing("fib", "", vec![(50, 1.20), (200, 1.10)])),
            trade_expiry: Some(drawing("exp", "trade-expiry", vec![(500, 1.0)])),
            blackout_pairs: vec![],
            news_pairs: vec![],
            sr_levels: vec![],
        }
    }

    #[test]
    fn short_invalidation_is_drawing_bound() {
        let roles = short_roles_full();
        let ctx = DispatchContext::default();
        let p = build_alert_spec("01-veto-too-high.yaml", Direction::Short, &roles, &ctx)
            .unwrap()
            .unwrap();
        match p {
            AlertPayload::Drawing {
                drawing_id,
                tool,
                condition_type,
                tv_name,
                ..
            } => {
                assert_eq!(drawing_id, "inv");
                assert_eq!(tool, Tool::LineToolHorzLine);
                assert_eq!(condition_type, ConditionType::CrossUp);
                assert_eq!(tv_name, "veto-too-high");
            }
            _ => panic!("expected Drawing variant, got {p:?}"),
        }
    }

    #[test]
    fn short_opposite_veto_is_pcl_price_value() {
        // For a short trade, `01-veto-too-low` is the pcl-exhausted
        // veto (not the invalidation). Value-bound to a price
        // computed from the fib.
        let roles = short_roles_full();
        let ctx = DispatchContext::default();
        let p = build_alert_spec("01-veto-too-low.yaml", Direction::Short, &roles, &ctx)
            .unwrap()
            .unwrap();
        match p {
            AlertPayload::PriceValue { value, tv_name, .. } => {
                // fib at [1.20, 1.10], short: midpoint=1.15, tp=1.00,
                // pcl = 1.15 + 0.8*(1.00-1.15) = 1.03.
                assert!((value - 1.03).abs() < 1e-9, "value = {value}");
                assert_eq!(tv_name, "veto-too-low");
            }
            _ => panic!("expected PriceValue, got {p:?}"),
        }
    }

    #[test]
    fn long_invalidation_is_too_low() {
        // Symmetric: for a long trade, the invalidation veto matches
        // `01-veto-too-low`.
        let roles = short_roles_full(); // labels don't matter for dispatch
        let ctx = DispatchContext::default();
        let p = build_alert_spec("01-veto-too-low.yaml", Direction::Long, &roles, &ctx)
            .unwrap()
            .unwrap();
        match p {
            AlertPayload::Drawing { condition_type, .. } => {
                assert_eq!(condition_type, ConditionType::CrossDown);
            }
            _ => panic!("expected Drawing, got {p:?}"),
        }
    }

    #[test]
    fn trade_expiry_drawing_bound_to_vertical_line() {
        let roles = short_roles_full();
        let ctx = DispatchContext::default();
        let p = build_alert_spec("02-veto-trade-expiry.yaml", Direction::Short, &roles, &ctx)
            .unwrap()
            .unwrap();
        match p {
            AlertPayload::Drawing { tool, .. } => {
                assert_eq!(tool, Tool::LineToolVertLine);
            }
            _ => panic!("expected Drawing, got {p:?}"),
        }
    }

    #[test]
    fn break_and_close_cross_direction_flips_with_trade() {
        let roles = short_roles_full();
        let ctx = DispatchContext::default();
        let p_short = build_alert_spec(
            "03-prep-break-and-close.yaml",
            Direction::Short,
            &roles,
            &ctx,
        )
        .unwrap()
        .unwrap();
        let p_long = build_alert_spec(
            "03-prep-break-and-close.yaml",
            Direction::Long,
            &roles,
            &ctx,
        )
        .unwrap()
        .unwrap();
        let cond_short = match p_short {
            AlertPayload::Drawing { condition_type, .. } => condition_type,
            _ => panic!("expected Drawing"),
        };
        let cond_long = match p_long {
            AlertPayload::Drawing { condition_type, .. } => condition_type,
            _ => panic!("expected Drawing"),
        };
        assert_eq!(cond_short, ConditionType::CrossDown);
        assert_eq!(cond_long, ConditionType::CrossUp);
    }

    #[test]
    fn retest_cross_direction_is_opposite_of_break() {
        let roles = short_roles_full();
        let ctx = DispatchContext::default();
        let p = build_alert_spec("04-prep-retest.yaml", Direction::Short, &roles, &ctx)
            .unwrap()
            .unwrap();
        if let AlertPayload::Drawing { condition_type, .. } = p {
            assert_eq!(condition_type, ConditionType::CrossUp);
        } else {
            panic!("expected Drawing");
        }
    }

    #[test]
    fn enter_uses_pine_plot_for_direction() {
        let roles = short_roles_full();
        let ctx = DispatchContext::default();
        let p_short = build_alert_spec("05-enter.yaml", Direction::Short, &roles, &ctx)
            .unwrap()
            .unwrap();
        let p_long = build_alert_spec("05-enter.yaml", Direction::Long, &roles, &ctx)
            .unwrap()
            .unwrap();
        match p_short {
            AlertPayload::PineAlertcondition {
                alert_cond_id,
                indicator_name,
                ..
            } => {
                assert_eq!(alert_cond_id, "plot_11");
                assert_eq!(indicator_name, "Candle Signals");
            }
            _ => panic!("expected PineAlertcondition for short enter"),
        }
        match p_long {
            AlertPayload::PineAlertcondition { alert_cond_id, .. } => {
                assert_eq!(alert_cond_id, "plot_10");
            }
            _ => panic!("expected PineAlertcondition for long enter"),
        }
    }

    #[test]
    fn close_on_reversal_uses_opposite_plot() {
        let roles = short_roles_full();
        let ctx = DispatchContext::default();
        let p = build_alert_spec("06-close-on-reversal.yaml", Direction::Short, &roles, &ctx)
            .unwrap()
            .unwrap();
        if let AlertPayload::PineAlertcondition { alert_cond_id, .. } = p {
            // Short trade closes on a Long Pattern reversal.
            assert_eq!(alert_cond_id, "plot_10");
        } else {
            panic!("expected PineAlertcondition");
        }
    }

    #[test]
    fn pause_alerts_bind_to_blackout_pair_edges() {
        let roles = short_roles_full();
        let start = drawing("bs", "blackout-start", vec![(300, 1.0)]);
        let end = drawing("be", "blackout-end", vec![(350, 1.0)]);
        let ctx = DispatchContext {
            blackout_pair: Some((&start, &end)),
            ..Default::default()
        };
        let p_start = build_alert_spec("01-pause-abc.yaml", Direction::Short, &roles, &ctx)
            .unwrap()
            .unwrap();
        let p_end = build_alert_spec("02-resume-abc.yaml", Direction::Short, &roles, &ctx)
            .unwrap()
            .unwrap();
        if let AlertPayload::Drawing { drawing_id, .. } = p_start {
            assert_eq!(drawing_id, "bs");
        } else {
            panic!("expected Drawing for pause");
        }
        if let AlertPayload::Drawing { drawing_id, .. } = p_end {
            assert_eq!(drawing_id, "be");
        } else {
            panic!("expected Drawing for resume");
        }
    }

    #[test]
    fn news_alerts_bind_to_news_pair_edges() {
        let roles = short_roles_full();
        let start = drawing("ns", "news-start", vec![(400, 1.0)]);
        let end = drawing("ne", "news-end", vec![(450, 1.0)]);
        let ctx = DispatchContext {
            news_pair: Some((&start, &end)),
            ..Default::default()
        };
        let p_start = build_alert_spec("01-news-start-eur.yaml", Direction::Short, &roles, &ctx)
            .unwrap()
            .unwrap();
        if let AlertPayload::Drawing { drawing_id, .. } = p_start {
            assert_eq!(drawing_id, "ns");
        } else {
            panic!("expected Drawing for news-start");
        }
    }

    #[test]
    fn calendar_window_picks_left_edge_for_pause_start() {
        let roles = short_roles_full();
        let ctx = DispatchContext {
            calendar_window: Some(CalendarWindow {
                start_iso: "2026-05-30T10:00:00Z".into(),
                end_iso: "2026-05-30T13:00:00Z".into(),
            }),
            ..Default::default()
        };
        let p = build_alert_spec("01-pause-cal-x-pause.yaml", Direction::Short, &roles, &ctx)
            .unwrap()
            .unwrap();
        if let AlertPayload::VertLineAt {
            base_time_epoch, ..
        } = p
        {
            // 2026-05-30T10:00:00Z = 1780135200 in epoch seconds.
            assert_eq!(base_time_epoch, 1780135200);
        } else {
            panic!("expected VertLineAt for calendar pause-start");
        }
    }

    #[test]
    fn calendar_window_picks_right_edge_for_resume() {
        let roles = short_roles_full();
        let ctx = DispatchContext {
            calendar_window: Some(CalendarWindow {
                start_iso: "2026-05-30T10:00:00Z".into(),
                end_iso: "2026-05-30T13:00:00Z".into(),
            }),
            ..Default::default()
        };
        let p = build_alert_spec("02-resume-cal-x-pause.yaml", Direction::Short, &roles, &ctx)
            .unwrap()
            .unwrap();
        if let AlertPayload::VertLineAt {
            base_time_epoch, ..
        } = p
        {
            assert_eq!(base_time_epoch, 1780146000);
        } else {
            panic!("expected VertLineAt for calendar resume");
        }
    }

    #[test]
    fn missing_role_returns_none() {
        let empty = Roles::default();
        let ctx = DispatchContext::default();
        let p = build_alert_spec("01-veto-too-high.yaml", Direction::Short, &empty, &ctx).unwrap();
        assert!(p.is_none());
    }

    #[test]
    fn unrecognized_basename_returns_none() {
        let roles = short_roles_full();
        let ctx = DispatchContext::default();
        let p = build_alert_spec("99-nonsense.yaml", Direction::Short, &roles, &ctx).unwrap();
        assert!(p.is_none());
    }

    #[test]
    fn json_payload_matches_expected_shape() {
        // The JS template reads `item.kind`, `item.drawing_id`, etc.
        // verbatim — verify the serialization is snake_case and that
        // the `kind` tag is set correctly.
        let roles = short_roles_full();
        let ctx = DispatchContext::default();
        let p = build_alert_spec("05-enter.yaml", Direction::Short, &roles, &ctx)
            .unwrap()
            .unwrap();
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["kind"], "pine_alertcondition");
        assert_eq!(v["alert_cond_id"], "plot_11");
        assert_eq!(v["indicator_name"], "Candle Signals");
        assert_eq!(v["frequency"], "on_bar_close");
    }

    #[test]
    fn drawing_payload_json_has_snake_case_condition() {
        let roles = short_roles_full();
        let ctx = DispatchContext::default();
        let p = build_alert_spec(
            "03-prep-break-and-close.yaml",
            Direction::Short,
            &roles,
            &ctx,
        )
        .unwrap()
        .unwrap();
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["kind"], "drawing");
        assert_eq!(v["condition_type"], "cross_down");
        assert_eq!(v["frequency"], "on_bar_close");
        assert_eq!(v["tool"], "LineToolTrendLine");
    }
}
