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
//!
//! ## ⚠ `granularity` is NOT here, and a `--spec-in` re-arm must still freeze it
//!
//! The chart's bar size feeds `TrendlineCross.bar_seconds`
//! (`trade_plan_build`), and every trendline price is interpolated in
//! **bar-index** space — so the same neckline read at H1 and at H4 yields
//! *different prices* at the same wall-clock instant. Today it comes from the
//! live chart (`resolution_to_granularity(state.resolution)`), which means a
//! re-arm off a chart left on a different timeframe would silently reprice the
//! whole neckline: plausible numbers, wrong plan, no error.
//!
//! It is deliberately **not** a field here — a chart resolution is a property of
//! how the setup was *read*, not of the setup's geometry, and this struct's scope
//! is the latter. But that makes it the enclosing frozen spec's job, and it must
//! not be forgotten there: `--spec-in` has to carry the granularity and either
//! use it directly or refuse when the live chart disagrees. Tracked in
//! `TODO-fixture-corpus.md` under commit 6e; `SCOPING-fixture-corpus.md` §3.3
//! already lists it in the Freeze column.

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
    /// `A` — the runup start.
    ///
    /// **No trigger reads this**, which is why it was initially left out. It is
    /// load-bearing anyway: `resolve_mw_trade_with_spread` uses it for the trade's
    /// **direction** (`mw_direction_from_anchors`) and for two rejection gates
    /// (`check_mw_structure`, and the 40%/50% `neckline_retrace_pct` depth gate).
    ///
    /// Leaving it out would tempt a spec-driven arm to infer direction from the
    /// pattern label instead — which agrees today, so tests would pass, while
    /// silently skipping the retracement gate and arming a setup a live arm would
    /// have **rejected**.
    pub runup_start: f64,
    /// `B` — the first peak (M) / trough (W).
    pub first_point: f64,
    /// `C` — the neckline.
    pub neckline: f64,
    /// `D` — the optional drawn right shoulder (4-point path). `None` is the
    /// 3-point form, so `right_shoulder.is_some()` replaces a `points.len() == 4`
    /// check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right_shoulder: Option<f64>,
    /// How many anchors the operator actually drew.
    ///
    /// Carried because the four named fields **cannot** represent it: a 5-anchor
    /// path is silently truncated to the first four during extraction, so it
    /// arrives here indistinguishable from a legitimate 4-anchor one. The arm
    /// gate rejects anything outside `3 | 4` — an over-long path means the
    /// operator drew something that isn't an M/W, and reading its first four
    /// points would arm a *different pattern than the one on screen*.
    ///
    /// This is the same class of omission as [`Self::runup_start`]: no trigger
    /// reads it, so it looks droppable, and dropping it fails **quietly** —
    /// tests pass, an arm succeeds, and the rejection just stops happening.
    ///
    /// `default = 3` so a spec written before this field loads as the minimal
    /// valid path rather than as `0` (which would reject every legacy spec).
    #[serde(default = "three_anchors")]
    pub anchors: usize,
}

/// Serde default for [`MwPath::anchors`] — see that field's doc.
fn three_anchors() -> usize {
    3
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
    /// Prices of the operator's **drawn** S/R horizontals, in chart order.
    ///
    /// **No trigger reads these** — same shape as [`MwPath::runup_start`], and the
    /// same reason they were nearly left out. They are load-bearing anyway:
    /// `build_sr_ranges` widens each into a `±reversal_band_pct` band, and
    /// `!spec.sr_reversal_ranges.is_empty()` decides whether the
    /// `07-close-on-sr-reversal` alert is emitted **at all**. Drop them and a
    /// position that would have closed for a partial win round-trips to its stop.
    ///
    /// This one fails *quietly*, which is what makes it worse than a plain missing
    /// field. The band vec is **half**-reconstructible: `tp_resistance_band` is
    /// derived from `fib_head_neckline` (present here) and is default-on, so a
    /// spec-in re-arm still produces a non-empty vec and still emits the alert —
    /// just without the operator's drawn levels. No error, no empty case, no
    /// missing rule; only a different exit price. Across a 291-trade grid that is
    /// exactly the "plausible numbers, wrong plan" failure the module doc warns
    /// about for granularity.
    ///
    /// Prices only, not `Anchor`s: `build_sr_ranges` reads `points.first()?.price`
    /// and ignores the time entirely (a horizontal has no meaningful `t`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sr_levels: Vec<f64>,
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
            // An M/W path drawing is preserved even when it is too SHORT to fill
            // the three required anchors — the missing ones read `f64::NAN` and
            // `anchors` carries the real count, which `check_mw_required` turns
            // into an operator-facing "found 2" rejection.
            //
            // The tempting shape here is `points.get(2)?` (collapsing a short path
            // to `None`, like every other role above). That is wrong for this one
            // field, because `mw_path.is_some()` is the **pattern discriminant**:
            // a `None` sends a half-drawn M/W down the *H&S* branch, where it is
            // reported as a pile of missing H&S drawings ("fib_retracement (TP)",
            // "trend_line labeled 'neckline'") rather than as the short M/W path
            // it actually is. Verified: that is exactly what happened when this
            // used `?`, and the misleading message was the regression.
            //
            // NAN is deliberate over 0.0 — no arithmetic downstream can produce a
            // plausible-looking result from it, and the gate rejects before any
            // of it is read anyway.
            mw_path: roles.mw_path.as_ref().map(|d| {
                let price_at = |i: usize| d.points.get(i).map_or(f64::NAN, |p| p.price);
                MwPath {
                    // `A` — needed for direction + the structure/retrace gates,
                    // not by any trigger.
                    runup_start: price_at(0),
                    first_point: price_at(1),
                    neckline: price_at(2),
                    right_shoulder: d.points.get(3).map(|p| p.price),
                    // The RAW count, before the truncation the four fields above
                    // impose — `check_mw_required` rejects anything outside 3|4,
                    // and it can only do that if the real number survives here.
                    anchors: d.points.len(),
                }
            }),
            // Drawn S/R horizontals — no trigger reads them, but they gate whether
            // `07-close-on-sr-reversal` is armed. `filter_map` mirrors
            // `build_sr_ranges`' own `points.first()` exactly: a degenerate
            // point-less drawing is skipped rather than defaulted.
            sr_levels: roles
                .sr_levels
                .iter()
                .filter_map(|d| d.points.first().map(|p| p.price))
                .collect(),
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

    /// `runup_start` is what decides M/W **direction**, so the geometry alone must
    /// determine it. A W (trough-first: A low, B high) is a LONG; an M (A high, B
    /// low) is a SHORT — and the same anchors also drive the retracement gate.
    ///
    /// This is the test that would have caught inferring direction from the pattern
    /// LABEL instead: that shortcut agrees with the anchors today, so it looks
    /// correct while silently bypassing `check_mw_structure` and the 40%/50%
    /// `neckline_retrace_pct` gate — arming a setup a live arm would have rejected.
    #[test]
    fn mw_direction_is_decided_by_the_anchors_not_a_label() {
        let path_of = |a: f64, b: f64, c: f64| {
            PlanGeometry::from_roles(&Roles {
                mw_path: Some(drawing(vec![pt(1, a), pt(2, b), pt(3, c)])),
                ..Default::default()
            })
            .mw_path
            .expect("path")
        };

        // W / iM: runup DOWN then up → long.
        let w = path_of(1.30, 1.00, 1.20);
        assert_eq!(
            crate::mw_geometry::mw_direction_from_anchors(w.runup_start, w.first_point),
            Some(trade_control_conventions::Direction::Long)
        );
        // M: runup UP then down → short.
        let m = path_of(1.00, 1.30, 1.10);
        assert_eq!(
            crate::mw_geometry::mw_direction_from_anchors(m.runup_start, m.first_point),
            Some(trade_control_conventions::Direction::Short)
        );
        // A flat first leg is undecidable — the arm must reject, not guess.
        let flat = path_of(1.20, 1.20, 1.10);
        assert_eq!(
            crate::mw_geometry::mw_direction_from_anchors(flat.runup_start, flat.first_point),
            None
        );

        // And the retracement gate is a function of all three anchors, so it can
        // only run when `runup_start` survived the extraction. Note the value is a
        // FRACTION despite the `_pct` name — `gate_neckline_pct` compares it against
        // 0.40 / 0.50 and only scales by 100 for the message.
        let frac =
            crate::mw_geometry::neckline_retrace_pct(m.runup_start, m.first_point, m.neckline);
        assert!(
            (frac - 2.0 / 3.0).abs() < 1e-9,
            "1.00 -> 1.30 retraced to 1.10 is 2/3 of the runup leg, got {frac}"
        );
        // 2/3 is past the 0.50 hard ceiling `gate_neckline_pct` enforces, so this
        // setup gets REJECTED at arm — exactly the gate a label-derived direction
        // would have skipped. (The gate itself is tested in `pipeline.rs`; here we
        // only pin that the geometry yields a value on the reject side of it.)
        assert!(
            frac > 0.50,
            "this fixture must land past the hard ceiling to be meaningful: {frac}"
        );
    }

    /// M/W reads path points [1]=B, [2]=C, and the OPTIONAL [3]=D right shoulder.
    #[test]
    fn mw_path_extracts_three_and_four_point_forms() {
        let three = Roles {
            mw_path: Some(drawing(vec![
                pt(1, 1.00), // A (runup start — no trigger reads it, but direction does)
                pt(2, 1.30), // B first point
                pt(3, 1.10), // C neckline
            ])),
            ..Default::default()
        };
        let g = PlanGeometry::from_roles(&three);
        let p = g.mw_path.expect("3-point path");
        // `A` must be carried: it decides DIRECTION and feeds the structure +
        // retracement gates, even though no trigger reads it.
        assert_eq!(p.runup_start, 1.00);
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
                runup_start: 1.00,
                first_point: 1.30,
                neckline: 1.10,
                right_shoulder: None,
                anchors: 3,
            }),
            sr_levels: vec![1.0950, 1.1250],
        };
        let back: PlanGeometry =
            serde_json::from_str(&serde_json::to_string_pretty(&g).unwrap()).unwrap();
        assert_eq!(g, back);
    }

    /// A spec written before `anchors` existed loads as a 3-anchor path — the
    /// minimal valid one — not as `0`, which the arm gate would reject.
    ///
    /// Without the serde default this is a hard load error, and with a plain
    /// `#[serde(default)]` it is a silent `0` that rejects every legacy spec at
    /// arm time. Both are worse than the explicit default.
    #[test]
    fn an_mw_path_without_anchors_defaults_to_three() {
        let legacy = r#"{"runup_start":1.0,"first_point":1.3,"neckline":1.1}"#;
        let p: MwPath = serde_json::from_str(legacy).expect("legacy spec must still load");
        assert_eq!(p.anchors, 3);
        assert!(p.right_shoulder.is_none());
    }

    /// A default geometry serializes thin (every field omitted), so a spec for a
    /// setup with few drawn roles stays readable.
    #[test]
    fn default_geometry_omits_every_key() {
        let json = serde_json::to_string(&PlanGeometry::default()).unwrap();
        assert_eq!(json, "{}");
    }
}
