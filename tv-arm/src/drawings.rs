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
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Point {
    /// UNIX seconds of the anchor.
    pub time: i64,
    /// Price level of the anchor.
    pub price: f64,
}

/// Optional drawing properties tv-mcp returns. The `text` field is
/// what the operator writes on the drawing to assign it a role.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Properties {
    /// Free-form text the operator typed on the drawing (used as the
    /// role label). Trimmed and lower-cased by the classifier.
    #[serde(default)]
    pub text: Option<String>,
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
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Drawing {
    /// tv-mcp's opaque drawing ID.
    pub id: String,
    /// Anchor points. One for horizontal/vertical lines; two for
    /// trend lines and fibs.
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
}

/// Vertical lines have a single anchor; `TimedAnchor` returns that
/// point's time. Lets [`crate::pair_lines::pair_vertical_lines`]
/// operate directly on `Drawing` without re-implementing the
/// anchor-time accessor.
impl TimedAnchor for Drawing {
    fn anchor_time(&self) -> i64 {
        // Single-anchor drawings use points[0]; multi-anchor ones
        // (none of the vertical lines we pair) fall back to the
        // earliest point so the sort key is still deterministic.
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
