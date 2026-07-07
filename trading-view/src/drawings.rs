//! Serde types mirroring the JSON shapes that tv-mcp's CLI emits.
//!
//! These are the structures used by `tv-arm` to talk to the
//! TradingView chart via the Node-side `tv-mcp` package. The shapes
//! are pinned to tv-mcp's current output and intentionally permissive
//! — fields we don't currently consume are tolerated via
//! `#[serde(default)]` so a tv-mcp version bump that adds a new
//! field doesn't break parsing.
//!
//! Three CLI calls produce these:
//! - `node tv-mcp draw list` → `ListDrawingsResponse` (`.shapes`)
//! - `node tv-mcp draw get <id>` → [`Drawing`]
//! - `node tv-mcp state` → [`ChartState`]

use serde::Deserialize;

use crate::pair_lines::TimedAnchor;

/// One drawing's anchor point. Time is UNIX seconds (tv-mcp emits
/// integer seconds even when the chart uses millisecond bars).
///
/// TradingView occasionally reads back a **degenerate** anchor — a
/// half-drawn or auto-extended drawing (e.g. a `parallel_channel`
/// whose third anchor hasn't resolved) emits `"price": null` (or, more
/// rarely, `"time": null`). A plain `f64`/`i64` field would make
/// `serde` reject the *entire* drawing, which historically aborted the
/// whole arm on one stray channel. So both coordinates tolerate `null`
/// by mapping it to a sentinel (`NaN` price / `0` time) and the point
/// is flagged [`Point::is_degenerate`]; the classifier declines any
/// drawing carrying such a point rather than crashing.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Point {
    /// UNIX seconds of the anchor. `0` is the sentinel for a `null`
    /// time read back from TradingView.
    #[serde(deserialize_with = "null_time_to_sentinel")]
    pub time: i64,
    /// Price level of the anchor. `NaN` is the sentinel for a `null`
    /// price read back from TradingView.
    #[serde(deserialize_with = "null_price_to_nan")]
    pub price: f64,
}

impl Point {
    /// Was this anchor read back incomplete (a `null` price or time)?
    /// Such a point can't be used for geometry, so a drawing that
    /// carries one is declined during role classification rather than
    /// silently dropped (dropping would shift the positional indices
    /// the H&S/fib geometry relies on).
    pub fn is_degenerate(&self) -> bool {
        self.price.is_nan() || self.time <= 0
    }
}

/// Map a `null` price to `NaN`; pass a real number through. Keeps the
/// `price` field a plain `f64` for every downstream consumer while
/// tolerating TradingView's degenerate readbacks.
fn null_price_to_nan<'de, D>(de: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<f64>::deserialize(de)?.unwrap_or(f64::NAN))
}

/// Map a `null` time to the `0` sentinel; pass a real epoch through.
fn null_time_to_sentinel<'de, D>(de: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<i64>::deserialize(de)?.unwrap_or(0))
}

/// Optional drawing properties tv-mcp returns. The `text` field is
/// what the operator writes on the drawing to assign it a role.
///
/// The `stop_level`/`profit_level` fields are emitted by the
/// long/short **position** tools (tv-mcp kind `long_position` /
/// `short_position`). They are *tick* distances from the entry anchor
/// (`points[0].price`), **not** absolute prices — the absolute SL/TP
/// are `entry ± level × tick_size` (see `tv-arm`'s position resolver).
/// They are `Option` because every other drawing kind omits them.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Properties {
    /// Free-form text the operator typed on the drawing (used as the
    /// role label). Trimmed and lower-cased by the classifier.
    #[serde(default)]
    pub text: Option<String>,
    /// Stop-loss distance from entry, in instrument *ticks*. Set only
    /// on position tools.
    #[serde(default, rename = "stopLevel")]
    pub stop_level: Option<f64>,
    /// Take-profit distance from entry, in instrument *ticks*. Set only
    /// on position tools.
    #[serde(default, rename = "profitLevel")]
    pub profit_level: Option<f64>,
    /// Position size the operator dialled in on the tool. Carried for
    /// visibility/logging; sizing is decided by the worker, not this.
    #[serde(default)]
    pub qty: Option<f64>,
}

/// A drawing stub from `draw list`. Carries just enough info to
/// decide whether to fetch the full drawing.
///
/// `name` is the tv-mcp "kind" string — e.g. `"trend_line"`,
/// `"horizontal_line"`, `"vertical_line"`, `"fib_retracement"`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DrawingStub {
    /// tv-mcp's opaque drawing ID.
    pub id: String,
    /// Drawing kind (e.g. `"trend_line"`).
    pub name: String,
}

/// A fully-detailed drawing from `draw get <id>`.
///
/// `draw list` returns each stub with an `id` field, but `draw get`
/// returns the same drawing with `entity_id`. `serde(alias)` accepts
/// either spelling so this struct works for both shapes.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Drawing {
    /// tv-mcp's opaque drawing ID.
    #[serde(alias = "entity_id")]
    pub id: String,
    /// Anchor points. One for horizontal/vertical lines; two for
    /// trend lines and fibs. Some drawing kinds (`long_position`,
    /// risk-reward tools) emit no points at all — `default` keeps
    /// the parse from failing on those.
    #[serde(default)]
    pub points: Vec<Point>,
    /// Drawing properties — notably `text` for role labels.
    #[serde(default)]
    pub properties: Properties,
}

impl Drawing {
    /// Trimmed label text, or empty string when absent.
    pub fn label(&self) -> &str {
        self.properties.text.as_deref().map(str::trim).unwrap_or("")
    }

    /// Does this drawing carry any degenerate anchor (a `null` price or
    /// time read back from TradingView)? Such a drawing — typically a
    /// half-drawn or auto-extending channel/fib the operator left on the
    /// chart — can't be reliably role-matched, so the classifier skips
    /// it instead of aborting the whole arm.
    pub fn has_degenerate_point(&self) -> bool {
        self.points.iter().any(Point::is_degenerate)
    }

    /// Latest anchor time across all the drawing's points. Used by
    /// the classifier to break ties when multiple drawings claim the
    /// same role — the newer one wins (older drawings are stale
    /// leftovers from prior setups).
    ///
    /// Returns `0` for a drawing with no points (shouldn't happen in
    /// practice but keeps the call total).
    pub fn latest_time(&self) -> i64 {
        self.points.iter().map(|p| p.time).max().unwrap_or(0)
    }

    /// Slice of every anchor's price — fed straight into the
    /// geometry helpers in [`crate::geometry`].
    pub fn prices(&self) -> Vec<f64> {
        self.points.iter().map(|p| p.price).collect()
    }

    /// Earliest anchor time across all the drawing's points (the start
    /// of its time-span). Mirror of [`Self::latest_time`]. Returns `0`
    /// for a point-less drawing.
    pub fn earliest_time(&self) -> i64 {
        self.points.iter().map(|p| p.time).min().unwrap_or(0)
    }

    /// Does this drawing's time-span **intersect** the visible window
    /// `[from, to]` (inclusive)?
    ///
    /// Intersection, not containment: a trend line with one anchor left
    /// of `from` and one right of `to` spans the whole view and counts as
    /// in-window. A single-anchor drawing (horizontal / vertical line)
    /// has a zero-width span, so this reduces to "its anchor sits in
    /// `[from, to]`". A drawing whose entire span is to the left of `from`
    /// or to the right of `to` does **not** intersect.
    ///
    /// A point-less drawing (no anchors) is treated as not in any window.
    pub fn intersects_window(&self, from: i64, to: i64) -> bool {
        if self.points.is_empty() {
            return false;
        }
        // Spans overlap iff each starts at or before the other ends.
        self.earliest_time() <= to && self.latest_time() >= from
    }
}

/// Vertical lines have a single anchor; `TimedAnchor` returns that
/// point's time so tv-arm's single-slot role pickers can sort/compare
/// drawings by anchor time without re-implementing the accessor.
impl TimedAnchor for Drawing {
    fn anchor_time(&self) -> i64 {
        // Single-anchor drawings use points[0]; multi-anchor ones
        // fall back to the earliest point so the sort key is still
        // deterministic.
        self.points.iter().map(|p| p.time).min().unwrap_or(0)
    }
}

/// `draw list` response — tv-mcp wraps the stubs in a `shapes`
/// field.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ListDrawingsResponse {
    /// The drawing stubs on the chart.
    pub shapes: Vec<DrawingStub>,
}

/// `state` response — what the chart is currently showing.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ChartState {
    /// Full TradingView symbol (`EXCHANGE:SYMBOL`, e.g. `OANDA:EURUSD`).
    pub symbol: String,
    /// Chart resolution (`"15"`, `"60"`, `"D"`, ...).
    pub resolution: String,
}

/// `info` response — symbol metadata. We only care about `description`
/// today (the human-readable name used as a TradeNation catalog
/// fallback when the bare symbol doesn't resolve), but the other
/// fields are exposed in case future strategies need them.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ChartInfo {
    /// Bare TradingView symbol (`EURUSD`, `USDCAD`).
    #[serde(default)]
    pub symbol: String,
    /// Human-readable description (`"EURO VS US DOLLAR"`,
    /// `"U.S. Dollar / Canadian Dollar"`). Empty when TV omits it.
    #[serde(default)]
    pub description: String,
    /// Full `EXCHANGE:SYMBOL` form. Matches `ChartState.symbol`.
    #[serde(default)]
    pub full_name: String,
    /// Type tag (`"forex"`, `"index"`, `"cfd"`, ...).
    #[serde(default, rename = "type")]
    pub kind: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_trend_line_drawing() {
        let json = r#"{
            "id": "abc123",
            "points": [
                {"time": 1700000000, "price": 1.10},
                {"time": 1700003600, "price": 1.12}
            ],
            "properties": {"text": "neckline"}
        }"#;
        let d: Drawing = serde_json::from_str(json).expect("parse");
        assert_eq!(d.id, "abc123");
        assert_eq!(d.points.len(), 2);
        assert_eq!(d.points[0].price, 1.10);
        assert_eq!(d.label(), "neckline");
        assert_eq!(d.latest_time(), 1700003600);
        assert_eq!(d.prices(), vec![1.10, 1.12]);
    }

    #[test]
    fn label_trims_whitespace() {
        let json =
            r#"{"id":"x","points":[{"time":1,"price":1.0}],"properties":{"text":"  retest  "}}"#;
        let d: Drawing = serde_json::from_str(json).unwrap();
        assert_eq!(d.label(), "retest");
    }

    #[test]
    fn missing_properties_is_empty_label() {
        // Some drawings come back without a `properties` block at all.
        let json = r#"{"id":"x","points":[{"time":1,"price":1.0}]}"#;
        let d: Drawing = serde_json::from_str(json).unwrap();
        assert_eq!(d.label(), "");
    }

    #[test]
    fn missing_text_field_is_empty_label() {
        let json = r#"{"id":"x","points":[{"time":1,"price":1.0}],"properties":{}}"#;
        let d: Drawing = serde_json::from_str(json).unwrap();
        assert_eq!(d.label(), "");
    }

    #[test]
    fn null_text_field_is_empty_label() {
        let json = r#"{"id":"x","points":[{"time":1,"price":1.0}],"properties":{"text":null}}"#;
        let d: Drawing = serde_json::from_str(json).unwrap();
        assert_eq!(d.label(), "");
    }

    #[test]
    fn timed_anchor_uses_min_point_time() {
        // Order of points in the JSON shouldn't matter — anchor_time
        // is the earliest, so a vertical line at t=200 sorts after
        // one at t=100 regardless of input ordering.
        let json = r#"{"id":"v1","points":[{"time":200,"price":1.0}]}"#;
        let d: Drawing = serde_json::from_str(json).unwrap();
        assert_eq!(d.anchor_time(), 200);
    }

    #[test]
    fn parses_drawing_stub() {
        let json = r#"{"id":"s1","name":"trend_line"}"#;
        let s: DrawingStub = serde_json::from_str(json).unwrap();
        assert_eq!(s.id, "s1");
        assert_eq!(s.name, "trend_line");
    }

    #[test]
    fn parses_list_response_with_extra_fields() {
        // Production tv-mcp output includes more than `shapes` —
        // `#[serde(default)]` posture means unknown fields are ignored.
        let json = r#"{
            "shapes": [
                {"id": "a", "name": "trend_line"},
                {"id": "b", "name": "fib_retracement"}
            ],
            "count": 2
        }"#;
        let resp: ListDrawingsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.shapes.len(), 2);
        assert_eq!(resp.shapes[1].name, "fib_retracement");
    }

    #[test]
    fn parses_chart_state() {
        let json = r#"{"symbol":"OANDA:EURUSD","resolution":"15"}"#;
        let s: ChartState = serde_json::from_str(json).unwrap();
        assert_eq!(s.symbol, "OANDA:EURUSD");
        assert_eq!(s.resolution, "15");
    }

    #[test]
    fn parses_horizontal_line_single_point() {
        // Horizontal lines have one anchor.
        let json = r#"{
            "id": "h1",
            "points": [{"time": 1700000000, "price": 1.234}],
            "properties": {"text": "too-high"}
        }"#;
        let d: Drawing = serde_json::from_str(json).unwrap();
        assert_eq!(d.points.len(), 1);
        assert_eq!(d.prices(), vec![1.234]);
        assert_eq!(d.label(), "too-high");
    }

    #[test]
    fn parses_short_position_levels() {
        // Trimmed from a real `draw get` on a DE40 short_position tool.
        // entry = points[0].price; stopLevel/profitLevel are TICK
        // distances, not absolute prices.
        let json = r##"{
            "entity_id": "R5zDSP",
            "points": [
                {"price": 23475, "time": 1773738000},
                {"price": 23475, "time": 1774227600}
            ],
            "properties": {
                "linecolor": "#808080",
                "stopLevel": 3000,
                "profitLevel": 7007,
                "qty": 0.0103,
                "accountSize": 10000
            },
            "name": "short_position"
        }"##;
        let d: Drawing = serde_json::from_str(json).expect("parse");
        assert_eq!(d.points[0].price, 23475.0);
        assert_eq!(d.properties.stop_level, Some(3000.0));
        assert_eq!(d.properties.profit_level, Some(7007.0));
        assert_eq!(d.properties.qty, Some(0.0103));
        // Position tools carry no role text.
        assert_eq!(d.label(), "");
    }

    #[test]
    fn non_position_drawing_has_no_levels() {
        let json =
            r#"{"id":"x","points":[{"time":1,"price":1.0}],"properties":{"text":"neckline"}}"#;
        let d: Drawing = serde_json::from_str(json).unwrap();
        assert_eq!(d.properties.stop_level, None);
        assert_eq!(d.properties.profit_level, None);
    }

    fn pts(times: &[i64]) -> Drawing {
        Drawing {
            id: "x".into(),
            points: times
                .iter()
                .map(|&t| Point {
                    time: t,
                    price: 1.0,
                })
                .collect(),
            properties: Properties::default(),
        }
    }

    #[test]
    fn intersects_window_single_anchor_inside() {
        assert!(pts(&[150]).intersects_window(100, 200));
        assert!(pts(&[100]).intersects_window(100, 200)); // on the from edge
        assert!(pts(&[200]).intersects_window(100, 200)); // on the to edge
    }

    #[test]
    fn intersects_window_single_anchor_outside() {
        assert!(!pts(&[50]).intersects_window(100, 200)); // left of window
        assert!(!pts(&[250]).intersects_window(100, 200)); // right of window
    }

    #[test]
    fn intersects_window_span_crossing_or_overlapping() {
        // Span fully inside.
        assert!(pts(&[120, 180]).intersects_window(100, 200));
        // Span straddles the whole view (one anchor each side).
        assert!(pts(&[50, 300]).intersects_window(100, 200));
        // Span overlaps the left edge only.
        assert!(pts(&[50, 150]).intersects_window(100, 200));
        // Span overlaps the right edge only.
        assert!(pts(&[150, 300]).intersects_window(100, 200));
    }

    #[test]
    fn intersects_window_span_fully_outside() {
        assert!(!pts(&[10, 90]).intersects_window(100, 200)); // entirely left
        assert!(!pts(&[210, 300]).intersects_window(100, 200)); // entirely right
    }

    #[test]
    fn intersects_window_pointless_drawing_is_never_in_window() {
        assert!(!pts(&[]).intersects_window(0, i64::MAX));
    }

    #[test]
    fn null_price_anchor_parses_as_degenerate() {
        // Real `parallel_channel` readback from TradingView: two valid
        // anchors and a third whose price came back null (auto-extending
        // third anchor not yet resolved). Before the fix this `null`
        // made serde reject the whole drawing, aborting the arm.
        let json = r#"{
            "entity_id": "kazo98",
            "points": [
                {"price": 0.80513, "time": 1772582400},
                {"price": 0.80513, "time": 1772582400},
                {"price": null, "time": 1772582400}
            ],
            "name": "parallel_channel"
        }"#;
        let d: Drawing = serde_json::from_str(json).expect("parse must survive null price");
        assert_eq!(d.points.len(), 3);
        assert!(d.points[0].price.is_finite());
        assert!(d.points[2].price.is_nan(), "null price → NaN sentinel");
        assert!(d.points[2].is_degenerate());
        assert!(!d.points[0].is_degenerate());
        assert!(
            d.has_degenerate_point(),
            "drawing with a null anchor is degenerate"
        );
    }

    #[test]
    fn null_time_anchor_is_degenerate() {
        let json = r#"{"id":"x","points":[{"time":null,"price":1.0}]}"#;
        let d: Drawing = serde_json::from_str(json).expect("parse must survive null time");
        assert_eq!(d.points[0].time, 0, "null time → 0 sentinel");
        assert!(d.points[0].is_degenerate());
        assert!(d.has_degenerate_point());
    }

    #[test]
    fn well_formed_drawing_is_not_degenerate() {
        let json = r#"{"id":"x","points":[{"time":1,"price":1.0},{"time":2,"price":2.0}]}"#;
        let d: Drawing = serde_json::from_str(json).unwrap();
        assert!(!d.has_degenerate_point());
    }

    #[test]
    fn parses_fib_retracement_two_endpoints() {
        // Fibs have exactly two anchor points (head and neckline).
        let json = r#"{
            "id": "f1",
            "points": [
                {"time": 1700000000, "price": 1.20},
                {"time": 1700100000, "price": 1.10}
            ],
            "properties": {"text": ""}
        }"#;
        let d: Drawing = serde_json::from_str(json).unwrap();
        assert_eq!(d.points.len(), 2);
        assert_eq!(d.label(), "");
        assert_eq!(d.prices(), vec![1.20, 1.10]);
    }
}
