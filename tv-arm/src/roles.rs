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
use tracing::info;
use trade_control_conventions::{
    BLACKOUT_END_LABELS, BLACKOUT_START_LABELS, BREAK_LABELS, INVALIDATION_LABELS, NEWS_END_LABELS,
    NEWS_START_LABELS, RETEST_LABELS, SR_LEVEL_LABELS, TRADE_EXPIRY_LABELS, matches,
};

use crate::drawings::{Drawing, DrawingStub};
use crate::tv_mcp::TvMcp;
use trading_view::pair_lines::pair_vertical_lines;

/// Drawing kinds emitted by tv-mcp.
mod kind {
    pub const HORIZONTAL_LINE: &str = "horizontal_line";
    pub const TREND_LINE: &str = "trend_line";
    pub const VERTICAL_LINE: &str = "vertical_line";
    pub const FIB_RETRACEMENT: &str = "fib_retracement";
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
    /// Support / resistance horizontal lines for the
    /// `07-close-on-sr-reversal` alert.
    pub sr_levels: Vec<Drawing>,
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
pub fn classify<F: DrawingFetcher>(fetcher: &F, stubs: &[DrawingStub]) -> Result<Roles> {
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

    for stub in stubs {
        let d = fetcher.get_drawing(&stub.id)?;
        let kind = stub.name.as_str();
        let lbl_owned = d.label().to_string();
        let lbl = lbl_owned.as_str();

        match kind {
            kind::HORIZONTAL_LINE if matches(lbl, INVALIDATION_LABELS) => {
                invalidations.push((d, lbl.to_lowercase()));
            }
            kind::HORIZONTAL_LINE if matches(lbl, SR_LEVEL_LABELS) => {
                sr_levels.push(d);
            }
            kind::TREND_LINE if matches(lbl, BREAK_LABELS) => break_lines.push(d),
            kind::TREND_LINE if matches(lbl, RETEST_LABELS) => retest_lines.push(d),
            kind::FIB_RETRACEMENT => tp_fibs.push(d),
            kind::VERTICAL_LINE if matches(lbl, TRADE_EXPIRY_LABELS) => {
                trade_expiries.push(d);
            }
            kind::VERTICAL_LINE if matches(lbl, BLACKOUT_START_LABELS) => {
                blackout_starts.push(d);
            }
            kind::VERTICAL_LINE if matches(lbl, BLACKOUT_END_LABELS) => {
                blackout_ends.push(d);
            }
            kind::VERTICAL_LINE if matches(lbl, NEWS_START_LABELS) => news_starts.push(d),
            kind::VERTICAL_LINE if matches(lbl, NEWS_END_LABELS) => news_ends.push(d),
            _ => {}
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

    Ok(roles)
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
    use crate::drawings::{Point, Properties};
    use std::collections::HashMap;

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

        let roles = classify(&mcp, &stubs).expect("classify ok");

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
        let roles = classify(&mcp, &stubs).expect("classify ok");
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
        let roles = classify(&mcp, &stubs).expect("ok");
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
        let roles = classify(&mcp, &stubs).expect("ok");
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
        let roles = classify(&mcp, &stubs).expect("ok");
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
        let roles = classify(&mcp, &stubs).expect("ok");
        assert!(roles.break_and_close.is_none());
        assert!(roles.retest.is_none());
    }

    #[test]
    fn odd_blackout_count_errors() {
        let (stubs, mcp) = fixture(vec![(
            stub("bs", "vertical_line"),
            drawing("bs", "blackout-start", vec![(300, 1.0)]),
        )]);
        let err = classify(&mcp, &stubs).unwrap_err();
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
        let err = classify(&mcp, &stubs).unwrap_err();
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
        let roles = classify(&mcp, &stubs).expect("ok");
        assert_eq!(roles.sr_levels.len(), 2);
    }

    #[test]
    fn fib_with_text_label_still_classified() {
        // Fibs match purely on kind — any text label is ignored.
        let (stubs, mcp) = fixture(vec![(
            stub("f", "fib_retracement"),
            drawing("f", "leftover note", vec![(50, 1.2), (200, 1.1)]),
        )]);
        let roles = classify(&mcp, &stubs).expect("ok");
        assert!(roles.tp_fib.is_some());
    }
}
