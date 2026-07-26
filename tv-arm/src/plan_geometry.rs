//! The plan's geometry as **plain data** — no TradingView drawings.
//!
//! `build_trade_plan` used to read raw `Drawing`s out of [`Roles`] while
//! assembling triggers. That coupled plan-building to a live chart: the only way
//! to rebuild a plan was to have the operator's drawings present, and *latest* on
//! the chart. Two consequences worth naming:
//!
//! - **A plan couldn't be rebuilt.** Redraw or delete the pattern and the plan is
//!   unreproducible. A frozen `TradePlan` can be replayed, but it can't answer
//!   "what would we do with this setup *today*" — it replays yesterday's
//!   plan-building forever, blind to any change in how we pick invalidation,
//!   compute TP, or lay out preps.
//! - **Wrong-drawing risk was silent.** Role resolution picks from whatever is on
//!   the chart; on a chart carrying several H&S patterns (we routinely had 3+,
//!   one from 2010) that yields confident, wrong output with no signal.
//!
//! This struct is the seam. Extract it once from the chart, and plan-building
//! reads *it* instead of the drawings. Freeze it and a plan can be rebuilt with
//! no TradingView at all — the operator confirms the correct pattern **once** and
//! no future run can pick the wrong drawing.
//!
//! ## Scope: what belongs here and what must NOT
//!
//! Only what `trade_plan_build::trigger_for` actually consumed. Deliberately
//! **not** the H&S *points* — the pattern points only ever existed to derive
//! these levels and lines, and they aren't always recoverable anyway.
//!
//! And nothing time-varying. These are properties of the **setup**:
//! geometry, levels, epochs. Things that are properties of *this moment* — the
//! broker spread, the live mid for a pullback anchor, the news calendar — must be
//! **re-read on every arm**, never frozen. A frozen spread mis-sizes an entry; a
//! frozen "price at arm time" is a contradiction. ATR isn't here either: the
//! engine computes it from candles, so there's nothing to carry.

use serde::{Deserialize, Serialize};
use trade_control_core::trade_plan::LinePoint;

use crate::roles::Roles;

/// One end of a trendline: when, and at what price. Mirrors the engine's
/// [`LinePoint`] as owned plain data.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    /// Unix epoch seconds of the anchor.
    pub at_epoch: i64,
    pub price: f64,
}

impl Anchor {
    pub fn new(at_epoch: i64, price: f64) -> Self {
        Self { at_epoch, price }
    }

    /// Lower into the engine's trigger form.
    pub fn to_line_point(self) -> LinePoint {
        LinePoint {
            at_epoch: self.at_epoch,
            price: self.price,
        }
    }
}

/// A two-anchor line (the neckline, in practice).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub a: Anchor,
    pub b: Anchor,
}

/// The M/W path anchors, as prices. Mirrors what `mw_price_trigger` read out of
/// `roles.mw_path.points`.
///
/// Note `MwSpec` on the CLI's `TradeSpec` already carried these — but
/// plan-building re-read them from the drawing anyway, so the spec copy was
/// decorative. This is the copy plan-building actually uses.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MwPath {
    /// `B` — the first peak (M) / trough (W).
    pub first_point: f64,
    /// `C` — the neckline.
    pub neckline: f64,
    /// `D` — the optional drawn right shoulder (4-point path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right_shoulder: Option<f64>,
}

/// Everything plan-building needs about the setup's shape.
///
/// Every field is `Option` because a role can legitimately be absent — the
/// corresponding rule is then skipped, exactly as a missing `Roles` entry used to
/// skip it. That "missing role → no rule" behaviour is load-bearing and preserved
/// verbatim.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PlanGeometry {
    /// The neckline: break-and-close crosses it one way, the retest crosses back.
    /// Both rules read this same line — a *drawn* retest trendline is deliberately
    /// ignored upstream (see `roles::classify`), so there is only one line here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub neckline: Option<Line>,
    /// The drawn invalidation level — `too-high` for a short, `too-low` for a
    /// long. A horizontal, so one price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalidation: Option<f64>,
    /// The fib's head and neckline readings, already resolved through the fib's
    /// `reverse` flag (NOT point order — that distinction caused two
    /// wrong-direction bugs; see `Drawing::fib_head_neckline`).
    ///
    /// TP is `2×neckline − head`; the pcl-exhausted abort is the ~80%-to-TP level
    /// derived from the same pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fib_head_neckline: Option<(f64, f64)>,
    /// Wall-clock epoch the trade-expiry veto fires at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trade_expiry_epoch: Option<i64>,
    /// Per-prep-step expiry epochs, by step name (`break-and-close`, `retest`).
    /// The CLI spec carries the step *names* only; these are the epochs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prep_expiry_epochs: Vec<(String, i64)>,
    /// The M/W path anchors, when this is an M/W trade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mw_path: Option<MwPath>,
}

impl PlanGeometry {
    /// Extract the geometry from live chart roles. The **only** place drawings are
    /// read for plan-building; everything downstream takes `&PlanGeometry`.
    ///
    /// Each extraction mirrors the previous inline `roles.*` read exactly,
    /// including its `?`-on-missing behaviour, so a plan built from this is
    /// byte-identical to one built from the drawings.
    pub fn from_roles(roles: &Roles) -> Self {
        Self {
            neckline: roles.break_and_close.as_ref().and_then(|d| {
                let (a, b) = (d.points.first()?, d.points.get(1)?);
                Some(Line {
                    a: Anchor::new(a.time, a.price),
                    b: Anchor::new(b.time, b.price),
                })
            }),
            // `horizontal_level` was `points.first()?.price`.
            invalidation: roles
                .invalidation
                .as_ref()
                .and_then(|d| d.points.first().map(|p| p.price)),
            // Resolved via the fib's `reverse` flag, not point order.
            fib_head_neckline: roles.tp_fib.as_ref().and_then(|d| d.fib_head_neckline()),
            // `time_trigger` was `points.first()?.time`.
            trade_expiry_epoch: roles
                .trade_expiry
                .as_ref()
                .and_then(|d| d.points.first().map(|p| p.time)),
            prep_expiry_epochs: roles
                .prep_expiries
                .iter()
                .filter_map(|(step, d)| Some((step.clone(), d.points.first()?.time)))
                .collect(),
            mw_path: roles.mw_path.as_ref().and_then(|d| {
                Some(MwPath {
                    first_point: d.points.get(1)?.price,
                    neckline: d.points.get(2)?.price,
                    right_shoulder: d.points.get(3).map(|p| p.price),
                })
            }),
        }
    }

    /// The expiry epoch for a named prep step, if drawn.
    pub fn prep_expiry(&self, step: &str) -> Option<i64> {
        self.prep_expiry_epochs
            .iter()
            .find(|(s, _)| s == step)
            .map(|(_, epoch)| *epoch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trading_view::drawings::{Drawing, Point, Properties};

    fn pt(time: i64, price: f64) -> Point {
        Point { time, price }
    }

    fn drawing(points: Vec<Point>) -> Drawing {
        Drawing {
            id: "d1".into(),
            points,
            properties: Properties::default(),
        }
    }

    #[test]
    fn empty_roles_extract_to_all_none() {
        let g = PlanGeometry::from_roles(&Roles::default());
        assert_eq!(g, PlanGeometry::default());
        assert!(g.neckline.is_none());
        assert!(g.prep_expiry_epochs.is_empty());
    }

    /// The neckline keeps BOTH anchors with their epochs — the engine interpolates
    /// the line per traded bar, and the A→B slope drives the retest tolerance, so
    /// dropping either anchor or its time would change behaviour.
    #[test]
    fn neckline_extracts_both_anchors_with_epochs() {
        let roles = Roles {
            break_and_close: Some(drawing(vec![pt(1000, 1.10), pt(2000, 1.12)])),
            ..Default::default()
        };
        let g = PlanGeometry::from_roles(&roles);
        let line = g.neckline.expect("neckline present");
        assert_eq!(line.a, Anchor::new(1000, 1.10));
        assert_eq!(line.b, Anchor::new(2000, 1.12));
        assert_eq!(line.a.to_line_point().at_epoch, 1000);
    }

    /// A one-point "trendline" can't make a line — extraction yields `None`, which
    /// downstream turns into "no rule", exactly as the old `points.get(1)?` did.
    #[test]
    fn degenerate_one_point_neckline_is_none() {
        let roles = Roles {
            break_and_close: Some(drawing(vec![pt(1000, 1.10)])),
            ..Default::default()
        };
        assert!(PlanGeometry::from_roles(&roles).neckline.is_none());
    }

    #[test]
    fn invalidation_and_expiry_read_the_first_point() {
        let roles = Roles {
            invalidation: Some(drawing(vec![pt(1000, 1.2345), pt(2000, 9.9)])),
            trade_expiry: Some(drawing(vec![pt(1784620800, 0.0)])),
            ..Default::default()
        };
        let g = PlanGeometry::from_roles(&roles);
        assert_eq!(g.invalidation, Some(1.2345));
        assert_eq!(g.trade_expiry_epoch, Some(1784620800));
    }

    /// The fib must resolve head/neckline through the `reverse` FLAG, not point
    /// order — reading point order caused two wrong-direction bugs.
    #[test]
    fn fib_resolves_through_the_reverse_flag() {
        let mut d = drawing(vec![pt(1000, 0.98367), pt(2000, 0.98861)]);
        // reverse == false → head is points[1].
        d.properties.reverse = Some(false);
        let g = PlanGeometry::from_roles(&Roles {
            tp_fib: Some(d.clone()),
            ..Default::default()
        });
        assert_eq!(g.fib_head_neckline, Some((0.98861, 0.98367)));

        // reverse == true flips it.
        d.properties.reverse = Some(true);
        let g = PlanGeometry::from_roles(&Roles {
            tp_fib: Some(d),
            ..Default::default()
        });
        assert_eq!(g.fib_head_neckline, Some((0.98367, 0.98861)));
    }

    #[test]
    fn prep_expiries_are_looked_up_by_step_name() {
        let roles = Roles {
            prep_expiries: vec![
                ("break-and-close".into(), drawing(vec![pt(111, 0.0)])),
                ("retest".into(), drawing(vec![pt(222, 0.0)])),
            ],
            ..Default::default()
        };
        let g = PlanGeometry::from_roles(&roles);
        assert_eq!(g.prep_expiry("break-and-close"), Some(111));
        assert_eq!(g.prep_expiry("retest"), Some(222));
        assert_eq!(g.prep_expiry("nope"), None);
    }

    /// M/W reads path points [1]=B, [2]=C, and the OPTIONAL [3]=D right shoulder.
    #[test]
    fn mw_path_extracts_three_and_four_point_forms() {
        let three = Roles {
            mw_path: Some(drawing(vec![
                pt(1, 1.00), // A (runup start — not used by the triggers)
                pt(2, 1.30), // B first point
                pt(3, 1.10), // C neckline
            ])),
            ..Default::default()
        };
        let g = PlanGeometry::from_roles(&three);
        let p = g.mw_path.expect("3-point path");
        assert_eq!((p.first_point, p.neckline), (1.30, 1.10));
        assert!(p.right_shoulder.is_none());

        let four = Roles {
            mw_path: Some(drawing(vec![
                pt(1, 1.00),
                pt(2, 1.30),
                pt(3, 1.10),
                pt(4, 1.35), // D drawn right shoulder
            ])),
            ..Default::default()
        };
        assert_eq!(
            PlanGeometry::from_roles(&four)
                .mw_path
                .and_then(|p| p.right_shoulder),
            Some(1.35)
        );
    }

    /// The whole struct round-trips through JSON — the property that makes a
    /// frozen, re-armable spec possible.
    #[test]
    fn geometry_json_round_trips() {
        let g = PlanGeometry {
            neckline: Some(Line {
                a: Anchor::new(1000, 1.10),
                b: Anchor::new(2000, 1.12),
            }),
            invalidation: Some(1.1500),
            fib_head_neckline: Some((1.0800, 1.1000)),
            trade_expiry_epoch: Some(1784620800),
            prep_expiry_epochs: vec![("retest".into(), 1784600000)],
            mw_path: Some(MwPath {
                first_point: 1.30,
                neckline: 1.10,
                right_shoulder: None,
            }),
        };
        let back: PlanGeometry =
            serde_json::from_str(&serde_json::to_string_pretty(&g).unwrap()).unwrap();
        assert_eq!(g, back);
    }

    /// A default geometry serializes thin (every field omitted), so a spec for a
    /// setup with few drawn roles stays readable.
    #[test]
    fn default_geometry_omits_every_key() {
        let json = serde_json::to_string(&PlanGeometry::default()).unwrap();
        assert_eq!(json, "{}");
    }
}
