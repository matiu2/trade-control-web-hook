//! Group chart drawings by the role the operator has assigned them.
//!
//! Port of `tv_arm_hs.py`'s `classify()` (lines ~189–263). The
//! operator labels each drawing with one of the vocabularies in
//! [`trade_control_conventions::labels`]; `classify()` walks the
//! drawings, dispatches by `(kind, label)`, and returns a [`Roles`]
//! struct ready for the alert pipeline.
//!
//! When multiple drawings claim the same single-slot role
//! (invalidation, neckline, retest, tp fib, trade-expiry), the
//! *latest* one wins — older drawings are stale leftovers from
//! prior setups. A note is logged at `info` so the operator can
//! notice they've left clutter on the chart.
//!
//! Blackout and news vertical-line pairs are collected as multi-slot
//! lists and chronologically paired via
//! [`trading_view::pair_lines::pair_vertical_lines`]; an odd count or a
//! reversed pair is a hard error so a misdrawn chart can't silently
//! arm half a window.

use color_eyre::eyre::Result;
use tracing::{debug, info};
use trade_control_conventions::{
    BLACKOUT_END_LABELS, BLACKOUT_START_LABELS, BREAK_LABELS, INVALIDATION_LABELS, NEWS_END_LABELS,
    NEWS_START_LABELS, RETEST_LABELS, SR_LEVEL_LABELS, TRADE_EXPIRY_LABELS, matches,
    prep_name_from_expiry_label,
};

use trading_view::drawings::{Drawing, DrawingStub};
use trading_view::mcp::TvMcp;
use trading_view::pair_lines::pair_vertical_lines;

/// Drawing kinds emitted by tv-mcp.
mod kind {
    pub const HORIZONTAL_LINE: &str = "horizontal_line";
    pub const TREND_LINE: &str = "trend_line";
    pub const VERTICAL_LINE: &str = "vertical_line";
    pub const FIB_RETRACEMENT: &str = "fib_retracement";
    /// The polyline / path tool used to mark an M/W reversal. It has no
    /// text property, so it's detected purely by geometry (3 anchors,
    /// all inside the visible range) — see [`super::classify`].
    pub const PATH: &str = "path";
    /// TradingView short-position tool. Entry is `points[0].price`;
    /// `stopLevel`/`profitLevel` (tick distances) come from properties.
    /// Detected purely by kind — the tool has no label.
    pub const SHORT_POSITION: &str = "short_position";
    /// TradingView long-position tool. Same shape as `SHORT_POSITION`.
    pub const LONG_POSITION: &str = "long_position";
}

/// Direction a position-tool drawing represents, read straight from the
/// drawing kind (`short_position` / `long_position`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionDirection {
    /// `long_position` — SL sits below entry, TP above.
    Long,
    /// `short_position` — SL sits above entry, TP below.
    Short,
}

/// A long/short position tool the operator drew on the chart. The
/// drawing carries an entry anchor (`points[0].price`) and tick-distance
/// `stopLevel`/`profitLevel` in its properties; the conversion to
/// absolute SL/TP prices lives in [`super::position_trade`].
#[derive(Debug, Clone)]
pub struct PositionDrawing {
    /// Long or short, from the drawing kind.
    pub direction: PositionDirection,
    /// The raw drawing — entry is `points[0].price`, levels are in
    /// `properties.stop_level` / `properties.profit_level`.
    pub drawing: Drawing,
}

/// Everything `classify()` extracts from the chart, grouped by role.
#[derive(Debug, Default, Clone)]
pub struct Roles {
    /// Invalidation veto drawing + its raw label (`"too-high"` /
    /// `"too-low"`). Both fields are set together or neither.
    pub invalidation: Option<Drawing>,
    /// Raw invalidation label, kept verbatim so downstream code can
    /// derive both the direction and the basename without reparsing.
    pub invalidation_label: Option<String>,
    /// Trend line for the neckline / break-and-close prep.
    pub break_and_close: Option<Drawing>,
    /// Trend line for the retest prep.
    pub retest: Option<Drawing>,
    /// Fib retracement that anchors the TP geometry.
    pub tp_fib: Option<Drawing>,
    /// Vertical line marking the trade-expiry veto.
    pub trade_expiry: Option<Drawing>,
    /// Chronologically paired blackout (pause/resume) windows.
    pub blackout_pairs: Vec<(Drawing, Drawing)>,
    /// Chronologically paired news-window pairs.
    pub news_pairs: Vec<(Drawing, Drawing)>,
    /// Support / resistance horizontal lines. Each one contributes
    /// an `[lo, hi]` price band to the consolidated
    /// `06-close-on-reversal` alert (`inside_window` gets `price`
    /// added; `sr_bands` carries the bands).
    pub sr_levels: Vec<Drawing>,
    /// Prep-expiry cutoff lines: each `<prep>-expiry` vertical line,
    /// resolved to its canonical prep step name (`break-and-close` /
    /// `retest`). One `prep-expire` alert is emitted per entry, bound
    /// to the drawing. Multiple lines for the same step keep only the
    /// latest (older ones are stale leftovers).
    pub prep_expiries: Vec<(String, Drawing)>,
    /// The M/W reversal path: a 3-anchor `path` drawing wholly inside
    /// the visible range. The path tool has no label, so detection is
    /// geometry-only — anchors are `[A (runup start), B (first
    /// point), C (neckline)]` in draw order. Latest-wins when several
    /// qualify. `None` for a plain H&S chart.
    pub mw_path: Option<Drawing>,
    /// A long/short **position** tool — the direct-entry path. Detected
    /// by kind alone (the tool has no label). Latest-wins when several
    /// are drawn. `None` unless the operator drew a position tool.
    pub position: Option<PositionDrawing>,
}

/// Anything that can hand us a `Drawing` by ID. The production impl is
/// [`TvMcp`]; tests provide an in-memory map.
pub trait DrawingFetcher {
    /// Fetch the full drawing for `entity_id`.
    fn get_drawing(&self, entity_id: &str) -> Result<Drawing>;
}

impl DrawingFetcher for TvMcp {
    fn get_drawing(&self, entity_id: &str) -> Result<Drawing> {
        TvMcp::get_drawing(self, entity_id)
    }
}

/// Walk `stubs`, fetch each drawing, and group by role. See module
/// docs for the resolution rules.
///
/// `visible_range` is the chart's currently-visible time window (unix
/// seconds). It's only consulted for M/W path detection: a `path`
/// drawing qualifies as the M/W marker only when **all** its anchors
/// fall inside that window, so a stale path scrolled off-screen is
/// ignored. Pass the window from [`trading_view::mcp::TvMcp::get_range`]
/// (`visible_range`).
pub fn classify<F: DrawingFetcher>(
    fetcher: &F,
    stubs: &[DrawingStub],
    visible_range: (i64, i64),
) -> Result<Roles> {
    let mut invalidations: Vec<(Drawing, String)> = Vec::new();
    let mut break_lines: Vec<Drawing> = Vec::new();
    let mut retest_lines: Vec<Drawing> = Vec::new();
    let mut tp_fibs: Vec<Drawing> = Vec::new();
    let mut trade_expiries: Vec<Drawing> = Vec::new();
    let mut blackout_starts: Vec<Drawing> = Vec::new();
    let mut blackout_ends: Vec<Drawing> = Vec::new();
    let mut news_starts: Vec<Drawing> = Vec::new();
    let mut news_ends: Vec<Drawing> = Vec::new();
    let mut sr_levels: Vec<Drawing> = Vec::new();
    // Prep-expiry lines, grouped by resolved canonical prep step so a
    // duplicate (re-armed) line keeps only the latest per step.
    let mut prep_expiry_lines: Vec<(&'static str, Drawing)> = Vec::new();
    // Candidate M/W paths: 3-anchor `path` drawings inside the visible
    // range. Latest-wins resolved after the loop.
    let mut mw_paths: Vec<Drawing> = Vec::new();
    // Candidate position tools (long/short). Latest-wins after the loop.
    let mut positions: Vec<PositionDrawing> = Vec::new();

    for stub in stubs {
        let d = fetcher.get_drawing(&stub.id)?;
        let kind = stub.name.as_str();
        let lbl_owned = d.label().to_string();
        let lbl = lbl_owned.as_str();

        let role = match kind {
            kind::HORIZONTAL_LINE if matches(lbl, INVALIDATION_LABELS) => {
                invalidations.push((d, lbl.to_lowercase()));
                Some("invalidation")
            }
            kind::HORIZONTAL_LINE if matches(lbl, SR_LEVEL_LABELS) => {
                sr_levels.push(d);
                Some("sr_level")
            }
            kind::TREND_LINE if matches(lbl, BREAK_LABELS) => {
                break_lines.push(d);
                Some("break_and_close")
            }
            kind::TREND_LINE if matches(lbl, RETEST_LABELS) => {
                retest_lines.push(d);
                Some("retest")
            }
            kind::FIB_RETRACEMENT => {
                tp_fibs.push(d);
                Some("tp_fib")
            }
            // Prep-expiry lines (`<prep>-expiry`) must be tested before
            // the trade-expiry arm: `trade-expiry` resolves to None here
            // (it's not a prep), so the two never collide, but checking
            // prep-expiry first keeps the intent obvious.
            kind::VERTICAL_LINE if prep_name_from_expiry_label(lbl).is_some() => {
                if let Some(step) = prep_name_from_expiry_label(lbl) {
                    prep_expiry_lines.push((step, d));
                }
                Some("prep_expiry")
            }
            kind::VERTICAL_LINE if matches(lbl, TRADE_EXPIRY_LABELS) => {
                trade_expiries.push(d);
                Some("trade_expiry")
            }
            kind::VERTICAL_LINE if matches(lbl, BLACKOUT_START_LABELS) => {
                blackout_starts.push(d);
                Some("blackout_start")
            }
            kind::VERTICAL_LINE if matches(lbl, BLACKOUT_END_LABELS) => {
                blackout_ends.push(d);
                Some("blackout_end")
            }
            kind::VERTICAL_LINE if matches(lbl, NEWS_START_LABELS) => {
                news_starts.push(d);
                Some("news_start")
            }
            kind::VERTICAL_LINE if matches(lbl, NEWS_END_LABELS) => {
                news_ends.push(d);
                Some("news_end")
            }
            // M/W reversal path: no label (the tool has no text field),
            // so it's accepted purely on geometry — exactly 3 anchors,
            // all inside the visible range. A path that's the wrong
            // shape or scrolled off-screen is ignored (logged below).
            kind::PATH if is_mw_path(&d, visible_range) => {
                mw_paths.push(d);
                Some("mw_path")
            }
            // Position tools have no label; direction is the kind. We
            // require an entry anchor and both tick levels so a
            // half-drawn tool is ignored rather than guessed at.
            kind::SHORT_POSITION | kind::LONG_POSITION if is_position(&d) => {
                let direction = if kind == kind::SHORT_POSITION {
                    PositionDirection::Short
                } else {
                    PositionDirection::Long
                };
                positions.push(PositionDrawing {
                    direction,
                    drawing: d,
                });
                Some("position")
            }
            _ => None,
        };
        match role {
            Some(r) => debug!(id = %stub.id, kind, label = lbl, role = r, "drawing classified"),
            None => debug!(
                id = %stub.id,
                kind,
                label = lbl,
                "drawing ignored — kind+label combination does not match any role vocabulary",
            ),
        }
    }

    let mut roles = Roles::default();

    if let Some((d, lbl)) = latest_with_label(invalidations, "invalidation") {
        roles.invalidation = Some(d);
        roles.invalidation_label = Some(lbl);
    }
    roles.break_and_close = latest_only(break_lines, "break_and_close");
    roles.retest = latest_only(retest_lines, "retest");
    roles.tp_fib = latest_only(tp_fibs, "tp_fib");
    roles.trade_expiry = latest_only(trade_expiries, "trade_expiry");

    roles.blackout_pairs = pair_vertical_lines(blackout_starts, blackout_ends, "blackout")?;
    roles.news_pairs = pair_vertical_lines(news_starts, news_ends, "news")?;
    roles.sr_levels = sr_levels;
    roles.prep_expiries = latest_prep_expiry_per_step(prep_expiry_lines);
    roles.mw_path = latest_only(mw_paths, "mw_path");
    roles.position = latest_position(positions);

    Ok(roles)
}

/// A position tool qualifies only when it has an entry anchor and both
/// tick levels — a half-drawn tool (no stop or no target) is ignored so
/// the pipeline never arms an entry missing a leg.
fn is_position(d: &Drawing) -> bool {
    !d.points.is_empty() && d.properties.stop_level.is_some() && d.properties.profit_level.is_some()
}

/// Latest-wins for position tools (the `PositionDrawing` wrapper means
/// `latest_only` can't be reused directly). Newest anchor time wins; a
/// note is logged when the operator left more than one on the chart.
fn latest_position(mut cands: Vec<PositionDrawing>) -> Option<PositionDrawing> {
    if cands.is_empty() {
        return None;
    }
    if cands.len() > 1 {
        info!(
            count = cands.len(),
            "multiple position tools on chart; picking the latest"
        );
    }
    cands.sort_by_key(|p| p.drawing.latest_time());
    cands.pop()
}

/// A `path` drawing qualifies as the M/W marker iff it has exactly 3
/// anchors and every anchor's time falls inside the visible range
/// `[from, to]` (inclusive). Off-screen paths are stale leftovers; a
/// path with 2 or 4+ anchors is a fat-fingered shape and is ignored
/// rather than guessed at.
fn is_mw_path(d: &Drawing, (from, to): (i64, i64)) -> bool {
    d.points.len() == 3 && d.points.iter().all(|p| p.time >= from && p.time <= to)
}

/// Collapse the per-step prep-expiry candidates to one drawing each —
/// the latest line wins (an earlier line on the same step is a stale
/// leftover from a prior arming). Returns `(canonical_step, drawing)`
/// pairs in stable step order (`break-and-close` before `retest`).
fn latest_prep_expiry_per_step(cands: Vec<(&'static str, Drawing)>) -> Vec<(String, Drawing)> {
    use trade_control_conventions::{PREP_BREAK_AND_CLOSE, PREP_RETEST};
    let mut out = Vec::new();
    for step in [PREP_BREAK_AND_CLOSE, PREP_RETEST] {
        let mut for_step: Vec<Drawing> = cands
            .iter()
            .filter(|(s, _)| *s == step)
            .map(|(_, d)| d.clone())
            .collect();
        if for_step.is_empty() {
            continue;
        }
        if for_step.len() > 1 {
            info!(
                count = for_step.len(),
                step, "multiple prep-expiry lines for step; picking the latest"
            );
        }
        for_step.sort_by_key(|d| d.latest_time());
        if let Some(d) = for_step.pop() {
            out.push((step.to_string(), d));
        }
    }
    out
}

/// Pick the drawing with the latest anchor time, logging a note when
/// duplicates were present.
fn latest_only(mut cands: Vec<Drawing>, role: &str) -> Option<Drawing> {
    if cands.is_empty() {
        return None;
    }
    if cands.len() > 1 {
        info!(
            count = cands.len(),
            role, "multiple drawings for role; picking the latest"
        );
    }
    cands.sort_by_key(|d| d.latest_time());
    cands.pop()
}

/// Variant of `latest_only` for drawings that carry an associated
/// label (currently just `invalidation`).
fn latest_with_label(mut cands: Vec<(Drawing, String)>, role: &str) -> Option<(Drawing, String)> {
    if cands.is_empty() {
        return None;
    }
    if cands.len() > 1 {
        info!(
            count = cands.len(),
            role, "multiple drawings for role; picking the latest"
        );
    }
    cands.sort_by_key(|(d, _)| d.latest_time());
    cands.pop()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use trading_view::drawings::{Point, Properties};

    /// A visible range wide enough to contain every H&S fixture anchor —
    /// M/W path detection is the only thing that consults it, and the
    /// H&S tests carry no paths, so any all-encompassing window works.
    const ANY_RANGE: (i64, i64) = (0, i64::MAX);

    /// In-memory fetcher backed by a HashMap<id, Drawing>.
    struct MockFetcher {
        drawings: HashMap<String, Drawing>,
    }

    impl DrawingFetcher for MockFetcher {
        fn get_drawing(&self, entity_id: &str) -> Result<Drawing> {
            self.drawings
                .get(entity_id)
                .cloned()
                .ok_or_else(|| color_eyre::eyre::eyre!("unknown id {entity_id}"))
        }
    }

    fn drawing(id: &str, label: &str, points: Vec<(i64, f64)>) -> Drawing {
        Drawing {
            id: id.to_string(),
            points: points
                .into_iter()
                .map(|(t, p)| Point { time: t, price: p })
                .collect(),
            properties: Properties {
                text: Some(label.to_string()),
                ..Default::default()
            },
        }
    }

    /// A position-tool drawing: single entry anchor + tick levels, no
    /// label (matches what tv-mcp emits for long/short position tools).
    fn position(id: &str, entry: f64, t: i64, stop_level: f64, profit_level: f64) -> Drawing {
        Drawing {
            id: id.to_string(),
            points: vec![Point {
                time: t,
                price: entry,
            }],
            properties: Properties {
                text: None,
                stop_level: Some(stop_level),
                profit_level: Some(profit_level),
                qty: Some(0.01),
            },
        }
    }

    fn stub(id: &str, name: &str) -> DrawingStub {
        DrawingStub {
            id: id.to_string(),
            name: name.to_string(),
        }
    }

    fn fixture(items: Vec<(DrawingStub, Drawing)>) -> (Vec<DrawingStub>, MockFetcher) {
        let mut stubs = Vec::new();
        let mut map = HashMap::new();
        for (s, d) in items {
            map.insert(s.id.clone(), d);
            stubs.push(s);
        }
        (stubs, MockFetcher { drawings: map })
    }

    #[test]
    fn classifies_full_short_h_and_s_chart() {
        // Realistic short-trade chart: invalidation cap, neckline,
        // retest, fib, trade-expiry, plus one blackout pair and one
        // news pair and one S/R level.
        let (stubs, mcp) = fixture(vec![
            (
                stub("inv", "horizontal_line"),
                drawing("inv", "too-high", vec![(100, 1.25)]),
            ),
            (
                stub("neck", "trend_line"),
                drawing("neck", "neckline", vec![(50, 1.10), (200, 1.10)]),
            ),
            (
                stub("re", "trend_line"),
                drawing("re", "retest", vec![(50, 1.10), (200, 1.10)]),
            ),
            (
                stub("fib", "fib_retracement"),
                drawing("fib", "", vec![(50, 1.20), (200, 1.10)]),
            ),
            (
                stub("exp", "vertical_line"),
                drawing("exp", "trade-expiry", vec![(500, 1.0)]),
            ),
            (
                stub("bs", "vertical_line"),
                drawing("bs", "blackout-start", vec![(300, 1.0)]),
            ),
            (
                stub("be", "vertical_line"),
                drawing("be", "blackout-end", vec![(350, 1.0)]),
            ),
            (
                stub("ns", "vertical_line"),
                drawing("ns", "news-start", vec![(400, 1.0)]),
            ),
            (
                stub("ne", "vertical_line"),
                drawing("ne", "news-end", vec![(450, 1.0)]),
            ),
            (
                stub("sr", "horizontal_line"),
                drawing("sr", "support", vec![(100, 1.05)]),
            ),
        ]);

        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("classify ok");

        assert!(roles.invalidation.is_some());
        assert_eq!(roles.invalidation_label.as_deref(), Some("too-high"));
        assert_eq!(roles.break_and_close.as_ref().unwrap().id, "neck");
        assert_eq!(roles.retest.as_ref().unwrap().id, "re");
        assert_eq!(roles.tp_fib.as_ref().unwrap().id, "fib");
        assert_eq!(roles.trade_expiry.as_ref().unwrap().id, "exp");

        assert_eq!(roles.blackout_pairs.len(), 1);
        assert_eq!(roles.blackout_pairs[0].0.id, "bs");
        assert_eq!(roles.blackout_pairs[0].1.id, "be");

        assert_eq!(roles.news_pairs.len(), 1);
        assert_eq!(roles.news_pairs[0].0.id, "ns");
        assert_eq!(roles.news_pairs[0].1.id, "ne");

        assert_eq!(roles.sr_levels.len(), 1);
        assert_eq!(roles.sr_levels[0].id, "sr");
    }

    #[test]
    fn empty_chart_yields_empty_roles() {
        let (stubs, mcp) = fixture(vec![]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("classify ok");
        assert!(roles.invalidation.is_none());
        assert!(roles.break_and_close.is_none());
        assert!(roles.blackout_pairs.is_empty());
        assert!(roles.news_pairs.is_empty());
        assert!(roles.sr_levels.is_empty());
    }

    #[test]
    fn duplicate_role_keeps_latest() {
        // Two necklines: old at t=100, new at t=500. Latest wins.
        let (stubs, mcp) = fixture(vec![
            (
                stub("old", "trend_line"),
                drawing("old", "neckline", vec![(50, 1.0), (100, 1.0)]),
            ),
            (
                stub("new", "trend_line"),
                drawing("new", "neckline", vec![(400, 1.0), (500, 1.0)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        assert_eq!(roles.break_and_close.unwrap().id, "new");
    }

    #[test]
    fn duplicate_invalidation_keeps_latest_with_label() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("old", "horizontal_line"),
                drawing("old", "too-low", vec![(100, 1.0)]),
            ),
            (
                stub("new", "horizontal_line"),
                drawing("new", "too-high", vec![(500, 1.5)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        assert_eq!(roles.invalidation.as_ref().unwrap().id, "new");
        assert_eq!(roles.invalidation_label.as_deref(), Some("too-high"));
    }

    #[test]
    fn label_aliases_accepted() {
        // `pause`/`resume` are aliases for `blackout-start`/`blackout-end`.
        let (stubs, mcp) = fixture(vec![
            (
                stub("ps", "vertical_line"),
                drawing("ps", "PAUSE", vec![(300, 1.0)]),
            ),
            (
                stub("pe", "vertical_line"),
                drawing("pe", "resume", vec![(350, 1.0)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        assert_eq!(roles.blackout_pairs.len(), 1);
    }

    #[test]
    fn unknown_label_is_ignored() {
        // A trend line with no recognized label shouldn't show up in
        // any role.
        let (stubs, mcp) = fixture(vec![(
            stub("a", "trend_line"),
            drawing("a", "scratchpad", vec![(50, 1.0), (100, 1.0)]),
        )]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        assert!(roles.break_and_close.is_none());
        assert!(roles.retest.is_none());
    }

    #[test]
    fn odd_blackout_count_errors() {
        let (stubs, mcp) = fixture(vec![(
            stub("bs", "vertical_line"),
            drawing("bs", "blackout-start", vec![(300, 1.0)]),
        )]);
        let err = classify(&mcp, &stubs, ANY_RANGE).unwrap_err();
        assert!(format!("{err}").contains("blackout"));
    }

    #[test]
    fn reversed_news_pair_errors() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("ns", "vertical_line"),
                drawing("ns", "news-start", vec![(500, 1.0)]),
            ),
            (
                stub("ne", "vertical_line"),
                drawing("ne", "news-end", vec![(400, 1.0)]),
            ),
        ]);
        let err = classify(&mcp, &stubs, ANY_RANGE).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("news"), "msg = {msg}");
        assert!(msg.contains("reversed"), "msg = {msg}");
    }

    #[test]
    fn multiple_sr_levels_all_kept() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("s1", "horizontal_line"),
                drawing("s1", "support", vec![(100, 1.05)]),
            ),
            (
                stub("s2", "horizontal_line"),
                drawing("s2", "resistance", vec![(200, 1.20)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        assert_eq!(roles.sr_levels.len(), 2);
    }

    #[test]
    fn prep_expiry_line_classifies_to_canonical_step() {
        // A `break-and-close-expiry` vertical line lands in prep_expiries
        // resolved to the canonical step name; a `trade-expiry` line on
        // the same chart stays a trade_expiry (no collision).
        let (stubs, mcp) = fixture(vec![
            (
                stub("bnce", "vertical_line"),
                drawing("bnce", "break-and-close-expiry", vec![(600, 1.0)]),
            ),
            (
                stub("exp", "vertical_line"),
                drawing("exp", "trade-expiry", vec![(900, 1.0)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        assert_eq!(roles.prep_expiries.len(), 1);
        assert_eq!(roles.prep_expiries[0].0, "break-and-close");
        assert_eq!(roles.prep_expiries[0].1.id, "bnce");
        assert_eq!(roles.trade_expiry.as_ref().unwrap().id, "exp");
    }

    #[test]
    fn duplicate_prep_expiry_keeps_latest() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("old", "vertical_line"),
                drawing("old", "retest-expiry", vec![(100, 1.0)]),
            ),
            (
                stub("new", "vertical_line"),
                drawing("new", "retest-expiry", vec![(500, 1.0)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        assert_eq!(roles.prep_expiries.len(), 1);
        assert_eq!(roles.prep_expiries[0].0, "retest");
        assert_eq!(roles.prep_expiries[0].1.id, "new");
    }

    #[test]
    fn fib_with_text_label_still_classified() {
        // Fibs match purely on kind — any text label is ignored.
        let (stubs, mcp) = fixture(vec![(
            stub("f", "fib_retracement"),
            drawing("f", "leftover note", vec![(50, 1.2), (200, 1.1)]),
        )]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        assert!(roles.tp_fib.is_some());
    }

    #[test]
    fn three_anchor_path_in_range_is_mw() {
        // A 3-anchor path, all anchors inside the visible window, with
        // no label — accepted as the M/W marker.
        let (stubs, mcp) = fixture(vec![(
            stub("p", "path"),
            drawing("p", "", vec![(100, 1.10), (200, 1.12), (300, 1.112)]),
        )]);
        let roles = classify(&mcp, &stubs, (50, 400)).expect("ok");
        assert_eq!(roles.mw_path.as_ref().unwrap().id, "p");
        // Anchors preserved in draw order = A, B, C.
        let pts = &roles.mw_path.unwrap().points;
        assert_eq!(pts[0].price, 1.10);
        assert_eq!(pts[2].price, 1.112);
    }

    #[test]
    fn path_partly_off_screen_is_ignored() {
        // Last anchor sits past the visible window → not the live
        // setup, ignore it (stale scrolled-away path).
        let (stubs, mcp) = fixture(vec![(
            stub("p", "path"),
            drawing("p", "", vec![(100, 1.10), (200, 1.12), (900, 1.112)]),
        )]);
        let roles = classify(&mcp, &stubs, (50, 400)).expect("ok");
        assert!(roles.mw_path.is_none());
    }

    #[test]
    fn two_anchor_path_is_ignored() {
        let (stubs, mcp) = fixture(vec![(
            stub("p", "path"),
            drawing("p", "", vec![(100, 1.10), (200, 1.12)]),
        )]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        assert!(roles.mw_path.is_none());
    }

    #[test]
    fn four_anchor_path_is_ignored() {
        let (stubs, mcp) = fixture(vec![(
            stub("p", "path"),
            drawing(
                "p",
                "",
                vec![(100, 1.1), (200, 1.12), (300, 1.11), (400, 1.13)],
            ),
        )]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        assert!(roles.mw_path.is_none());
    }

    #[test]
    fn duplicate_mw_paths_keep_latest() {
        // Two valid 3-anchor paths in range; the one with the later
        // anchor time wins (older is a stale leftover).
        let (stubs, mcp) = fixture(vec![
            (
                stub("old", "path"),
                drawing("old", "", vec![(100, 1.1), (150, 1.12), (200, 1.112)]),
            ),
            (
                stub("new", "path"),
                drawing("new", "", vec![(600, 1.1), (650, 1.12), (700, 1.112)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, (50, 800)).expect("ok");
        assert_eq!(roles.mw_path.unwrap().id, "new");
    }

    #[test]
    fn short_position_classifies_with_direction() {
        let (stubs, mcp) = fixture(vec![(
            stub("sp", "short_position"),
            position("sp", 23475.0, 1773738000, 3000.0, 7007.0),
        )]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        let pos = roles.position.expect("position present");
        assert_eq!(pos.direction, PositionDirection::Short);
        assert_eq!(pos.drawing.points[0].price, 23475.0);
        assert_eq!(pos.drawing.properties.stop_level, Some(3000.0));
        assert_eq!(pos.drawing.properties.profit_level, Some(7007.0));
    }

    #[test]
    fn long_position_classifies_with_direction() {
        let (stubs, mcp) = fixture(vec![(
            stub("lp", "long_position"),
            position("lp", 24195.3, 1778702400, 801.0, 2223.0),
        )]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        assert_eq!(roles.position.unwrap().direction, PositionDirection::Long);
    }

    #[test]
    fn half_drawn_position_without_levels_is_ignored() {
        // A position tool missing its profit level isn't a complete
        // setup — ignore it rather than arm a TP-less entry.
        let half = Drawing {
            id: "h".into(),
            points: vec![Point {
                time: 100,
                price: 100.0,
            }],
            properties: Properties {
                text: None,
                stop_level: Some(50.0),
                profit_level: None,
                qty: None,
            },
        };
        let (stubs, mcp) = fixture(vec![(stub("h", "short_position"), half)]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        assert!(roles.position.is_none());
    }

    #[test]
    fn duplicate_positions_keep_latest() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("old", "short_position"),
                position("old", 100.0, 100, 30.0, 70.0),
            ),
            (
                stub("new", "long_position"),
                position("new", 200.0, 900, 40.0, 80.0),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE).expect("ok");
        let pos = roles.position.unwrap();
        assert_eq!(pos.drawing.id, "new");
        assert_eq!(pos.direction, PositionDirection::Long);
    }
}
