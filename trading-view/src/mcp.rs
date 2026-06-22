//! Subprocess wrapper around the Node-side `tradingview-mcp-jackson`
//! CLI. Mirrors the Python `tv()` / `list_drawings()` / `get_drawing()`
//! / `get_state()` helpers in `tv_arm_hs.py`.
//!
//! Every call shells `node <root>/src/cli/index.js <args>`, captures
//! stdout, and parses the JSON. Errors surface as `eyre`-wrapped
//! reports carrying the exit status, stderr tail, and the args that
//! were attempted so the operator can re-run by hand.
//!
//! The Node module is *not* vendored — its path is hard-coded to
//! the operator's local checkout (matching the Python script). Slice
//! 5's `args.rs` will add a `--tv-mcp-root` override.

use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{Result, WrapErr, eyre};
use serde::de::DeserializeOwned;

use crate::drawings::{ChartState, Drawing, DrawingStub, ListDrawingsResponse};
use crate::symbol_info::SymbolInfo;

/// Default tv-mcp checkout location — matches `tv_arm_hs.py`'s
/// `TV_MCP_ROOT` constant.
pub const DEFAULT_TV_MCP_ROOT: &str = "/home/matiu/Downloads/tradingview-mcp-jackson";

/// Handle for invoking the tv-mcp CLI. Holds the module root so the
/// path to `src/cli/index.js` can be derived on each call.
#[derive(Debug, Clone)]
pub struct TvMcp {
    root: PathBuf,
}

impl TvMcp {
    /// Construct with an explicit module root. Use [`Self::default`]
    /// for the hard-coded default.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Path to the Node CLI entry-point.
    pub fn cli_path(&self) -> PathBuf {
        self.root.join("src").join("cli").join("index.js")
    }

    /// Root of the tv-mcp module — needed by `create_alerts.rs` when
    /// it renders the JS template that imports from this directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Run `node <cli> <args>` and parse stdout as JSON. The args are
    /// forwarded verbatim — no shell interpolation, no quoting.
    pub fn call_json<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T> {
        let cli = self.cli_path();
        let output = Command::new("node")
            .arg(&cli)
            .args(args)
            .output()
            .wrap_err_with(|| format!("failed to spawn `node {}`", cli.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!(
                "tv-mcp `{}` failed (exit {}): {}",
                args.join(" "),
                output
                    .status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                stderr.trim()
            ));
        }
        serde_json::from_slice::<T>(&output.stdout).wrap_err_with(|| {
            format!(
                "tv-mcp `{}` returned non-JSON output: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stdout)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )
        })
    }

    /// `draw list` — every drawing currently on the chart.
    pub fn list_drawings(&self) -> Result<Vec<DrawingStub>> {
        let resp: ListDrawingsResponse = self.call_json(&["draw", "list"])?;
        Ok(resp.shapes)
    }

    /// `draw get <id>` — fetch one drawing in full.
    pub fn get_drawing(&self, entity_id: &str) -> Result<Drawing> {
        self.call_json(&["draw", "get", entity_id])
    }

    /// `state` — the current chart's symbol and resolution.
    pub fn get_state(&self) -> Result<ChartState> {
        self.call_json(&["state"])
    }

    /// `info` — TradingView's symbol-info dialog payload for the
    /// current chart. The `description` field carries the broker's
    /// own name for the asset, which tv-arm uses to recover when the
    /// chart's bare TV symbol isn't in the instrument-lookup catalog.
    pub fn get_symbol_info(&self) -> Result<SymbolInfo> {
        self.call_json(&["info"])
    }

    /// `range` — the chart's currently-visible time window plus
    /// underlying bar coverage. Used by `tv-news` to scope its
    /// calendar query to what the operator is actually looking at.
    pub fn get_range(&self) -> Result<crate::range::ChartRange> {
        self.call_json(&["range"])
    }

    /// Draw a vertical line on the chart anchored at `time` (UNIX
    /// seconds), labeled with `text`. Returns the new drawing's
    /// entity-id so the caller can verify it landed.
    ///
    /// `price` doesn't matter for vertical lines (TV ignores it for
    /// alert evaluation) but the CLI requires a value — pass any
    /// non-NaN number such as the current bid mid.
    pub fn draw_vertical_line(&self, time: i64, price: f64, text: &str) -> Result<DrawShapeResult> {
        self.call_json(&[
            "draw",
            "shape",
            "-t",
            "vertical_line",
            "--time",
            &time.to_string(),
            "-p",
            &price.to_string(),
            "--text",
            text,
        ])
    }

    /// Draw a filled rectangle spanning the two corners in `rect`, tinted
    /// its `color` (a `#rrggbb` string used for both border and fill) at
    /// its `transparency` (0 = opaque, 100 = invisible) and labelled with
    /// its `text`. Returns the new drawing's entity-id.
    ///
    /// This is the primitive `replay-candles --annotate` draws positions
    /// with: TradingView's native position tool can't be created through
    /// tv-mcp (`createMultipointShape` silently no-ops for it), but the
    /// rectangle lands cleanly, so a position is two rectangles
    /// (entry→TP green, entry→SL red).
    pub fn draw_rectangle(&self, rect: &Rect<'_>) -> Result<DrawShapeResult> {
        let overrides = rectangle_overrides(rect.color, rect.transparency);
        self.call_json(&[
            "draw",
            "shape",
            "-t",
            "rectangle",
            "--time",
            &rect.time1.to_string(),
            "-p",
            &rect.price1.to_string(),
            "--time2",
            &rect.time2.to_string(),
            "--price2",
            &rect.price2.to_string(),
            "--overrides",
            &overrides,
            "--text",
            rect.text,
        ])
    }

    /// `draw remove <id>` — delete one drawing by entity-id. Used to
    /// clear prior `--annotate` rectangles before redrawing.
    pub fn remove_drawing(&self, entity_id: &str) -> Result<RemoveDrawingResult> {
        self.call_json(&["draw", "remove", entity_id])
    }
}

/// Result of `tv-mcp draw shape`. The CLI returns `{success, shape,
/// entity_id}` on success and `{success: false, error: "..."}` on
/// failure — both are normalized here so the caller can check
/// `success` once.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DrawShapeResult {
    /// Whether the shape landed on the chart.
    pub success: bool,
    /// The shape kind that was drawn (echoed from the input).
    #[serde(default)]
    pub shape: Option<String>,
    /// tv-mcp's opaque ID of the new drawing.
    #[serde(default)]
    pub entity_id: Option<String>,
    /// Error message when `success: false`.
    #[serde(default)]
    pub error: Option<String>,
}

/// A rectangle to draw: two opposite corners `(time1, price1)` →
/// `(time2, price2)` (UNIX seconds / absolute prices), a `#rrggbb`
/// `color` for border + fill, a fill `transparency` (0 opaque … 100
/// invisible), and `text` label. Bundles [`TvMcp::draw_rectangle`]'s
/// inputs so the call site reads as one shape, not eight positionals.
#[derive(Debug, Clone, Copy)]
pub struct Rect<'a> {
    /// First corner time (UNIX seconds).
    pub time1: i64,
    /// First corner price (absolute).
    pub price1: f64,
    /// Opposite corner time (UNIX seconds).
    pub time2: i64,
    /// Opposite corner price (absolute).
    pub price2: f64,
    /// `#rrggbb` tint for both border and fill.
    pub color: &'a str,
    /// Fill transparency: 0 opaque … 100 invisible.
    pub transparency: u8,
    /// Label text written on the drawing (carries the `replay:` tag).
    pub text: &'a str,
}

/// Build the `--overrides` JSON for a rectangle: tint both the border
/// (`color`) and fill (`backgroundColor`) the same hue, at the given
/// fill `transparency` (0 opaque … 100 invisible). Kept separate so the
/// JSON shape is unit-testable without shelling out to Node.
fn rectangle_overrides(color: &str, transparency: u8) -> String {
    format!("{{\"color\":{color:?},\"backgroundColor\":{color:?},\"transparency\":{transparency}}}")
}

/// Result of `tv-mcp draw remove`. The CLI returns
/// `{success, entity_id, removed, remaining_shapes}` — `removed` is the
/// load-bearing field (whether the shape is actually gone).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RemoveDrawingResult {
    /// Whether the CLI call itself succeeded.
    pub success: bool,
    /// Whether the shape is actually gone from the chart.
    #[serde(default)]
    pub removed: bool,
    /// Count of drawings still on the chart afterwards.
    #[serde(default)]
    pub remaining_shapes: Option<usize>,
}

impl Default for TvMcp {
    fn default() -> Self {
        Self::new(PathBuf::from(DEFAULT_TV_MCP_ROOT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_path_joins_root() {
        let mcp = TvMcp::new(PathBuf::from("/tmp/foo"));
        assert_eq!(mcp.cli_path(), PathBuf::from("/tmp/foo/src/cli/index.js"));
    }

    #[test]
    fn default_root_matches_python() {
        let mcp = TvMcp::default();
        assert_eq!(mcp.root(), Path::new(DEFAULT_TV_MCP_ROOT));
    }

    #[test]
    fn rectangle_overrides_tints_border_and_fill_same_color() {
        let o = rectangle_overrides("#26a69a", 80);
        assert_eq!(
            o,
            r##"{"color":"#26a69a","backgroundColor":"#26a69a","transparency":80}"##
        );
        // Must be valid JSON the Node side can `JSON.parse`.
        let parsed: serde_json::Value = serde_json::from_str(&o).expect("valid json");
        assert_eq!(parsed["color"], "#26a69a");
        assert_eq!(parsed["transparency"], 80);
    }

    #[test]
    fn parses_remove_drawing_result() {
        // Trimmed from a real `draw remove` response.
        let json = r#"{"success":true,"entity_id":"VI61Fw","removed":true,"remaining_shapes":55}"#;
        let r: RemoveDrawingResult = serde_json::from_str(json).expect("parse");
        assert!(r.success);
        assert!(r.removed);
        assert_eq!(r.remaining_shapes, Some(55));
    }

    #[test]
    fn missing_node_root_surfaces_error() {
        // The Node CLI doesn't exist at this path — node will still
        // run but exit non-zero. We only care that we get an Err
        // back, not a panic.
        let mcp = TvMcp::new(PathBuf::from("/tmp/does-not-exist-tv-arm-test"));
        let res: Result<ChartState> = mcp.call_json(&["state"]);
        assert!(res.is_err(), "expected error, got {res:?}");
    }
}
