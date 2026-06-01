//! Shared TradingView interop for the trade-control workspace.
//!
//! Wraps the `tradingview-mcp-jackson` Node CLI (subprocess) and
//! provides the serde types for the JSON shapes it emits. Consumed by
//! both `tv-arm` (writes alerts + drawings) and the upcoming
//! `tv-news` (reads chart range, draws calendar bars).
//!
//! What lives here:
//!
//! - `mcp` — the `TvMcp` subprocess wrapper (state, draw list, draw
//!   get, draw shape, range).
//! - `drawings` — the serde shapes for tv-mcp's JSON output
//!   (`DrawingStub`, `Drawing`, `ChartState`, etc.).
//! - `pair_lines` — pairing logic for vertical lines that come in
//!   ordered pairs (blackout-start / blackout-end, news-start /
//!   news-end). The `TimedAnchor` trait lets `Drawing` plug in
//!   without re-implementing the anchor-time accessor.
//!
//! What does **not** live here: anything strategy-specific. The
//! H&S role classifier (`tv-arm/src/roles.rs`) consumes from this
//! crate but doesn't belong inside it.
