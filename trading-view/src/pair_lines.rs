//! Chronologically pair `start` / `end` vertical-line markers into
//! `(start, end)` windows.
//!
//! Used for both blackout (pause/resume) and news-window
//! (news-start / news-end) lines.
//!
//! Starts and ends are sorted independently and zipped by index (the
//! i-th start pairs with the i-th end). A pair whose start and end
//! **snapped to the same timestamp** is a zero-length window — it arms
//! nothing — and is silently dropped rather than aborting the arm.
//! This was the 2026-06-26 DELL failure: TradingView snaps a drawn
//! vertical line to its bar's timestamp on readback, so two distinct
//! planned times in the same bar come back identical; a `resume` line
//! and the next event's `pause` line that land on the same bar zipped
//! into a `(t, t)` pair that the old code rejected as a "reversed pair"
//! and bailed the whole arm. Index-zip pairs touching windows correctly
//! (A's end == B's start gives `(.., t)` and `(t, ..)`, distinct pairs)
//! and only the genuinely degenerate `start == end` window is dropped.
//!
//! Still hard-errors when the start and end counts differ (a genuine
//! orphan line on the chart) or when a pair is reversed (`start > end`,
//! the operator drew the lines out of order).
//!
//! The implementation is generic over an "anchored at time T"
//! trait, so this module needs no knowledge of the tv-mcp Drawing
//! shape.

use color_eyre::eyre::{Result, eyre};

/// Anything that carries a single timestamp anchor — vertical lines
/// have one point, so `anchor_time` is just that point's
/// time-in-seconds.
pub trait TimedAnchor {
    /// UNIX seconds of the line's anchor.
    fn anchor_time(&self) -> i64;
}

/// Pair `starts` and `ends` chronologically (i-th start with i-th end).
///
/// `kind` is the label used in error messages (`"blackout"` /
/// `"news"`) so the operator can find the misdrawn line on the
/// chart.
///
/// Zero-length windows (a start and end that snapped to the same
/// timestamp) are silently dropped — they arm nothing — so a calendar
/// auto-draw whose lines collide on a bar boundary no longer aborts the
/// arm. A genuinely reversed pair (`start > end`) is still a hard error.
pub fn pair_vertical_lines<T: TimedAnchor>(
    mut starts: Vec<T>,
    mut ends: Vec<T>,
    kind: &str,
) -> Result<Vec<(T, T)>> {
    if starts.len() != ends.len() {
        return Err(eyre!(
            "{kind} lines must come in matched start/end pairs; \
             found {} start(s) and {} end(s). \
             Fix the chart (add the missing line or relabel) and re-run.",
            starts.len(),
            ends.len(),
        ));
    }
    starts.sort_by_key(|d| d.anchor_time());
    ends.sort_by_key(|d| d.anchor_time());
    let mut pairs = Vec::with_capacity(starts.len());
    for (i, (s, e)) in starts.into_iter().zip(ends).enumerate() {
        match s.anchor_time().cmp(&e.anchor_time()) {
            // Zero-length window: the start and end snapped to the same
            // bar on readback. Nothing to arm — drop it (don't error).
            std::cmp::Ordering::Equal => continue,
            // Reversed: the operator drew the end before the start. A
            // genuine chart mistake — surface it.
            std::cmp::Ordering::Greater => {
                return Err(eyre!(
                    "{kind} pair #{} is reversed: start={} is after end={}. \
                     Each {kind}-start must precede its {kind}-end.",
                    i + 1,
                    s.anchor_time(),
                    e.anchor_time(),
                ));
            }
            std::cmp::Ordering::Less => pairs.push((s, e)),
        }
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct Anchor {
        id: &'static str,
        time: i64,
    }

    impl TimedAnchor for Anchor {
        fn anchor_time(&self) -> i64 {
            self.time
        }
    }

    fn a(id: &'static str, time: i64) -> Anchor {
        Anchor { id, time }
    }

    #[test]
    fn pairs_in_chronological_order() {
        // Inputs deliberately out of order. Expected: zipped by time.
        let starts = vec![a("s2", 200), a("s1", 100)];
        let ends = vec![a("e2", 250), a("e1", 150)];
        let pairs = pair_vertical_lines(starts, ends, "blackout").expect("pairs ok");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0.id, "s1");
        assert_eq!(pairs[0].1.id, "e1");
        assert_eq!(pairs[1].0.id, "s2");
        assert_eq!(pairs[1].1.id, "e2");
    }

    #[test]
    fn errors_on_mismatched_counts() {
        let starts = vec![a("s1", 100)];
        let ends: Vec<Anchor> = vec![];
        let err = pair_vertical_lines(starts, ends, "news").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("news"));
        assert!(msg.contains("1 start"));
        assert!(msg.contains("0 end"));
    }

    #[test]
    fn errors_on_reversed_pair() {
        // A genuinely reversed pair (end drawn before start) is still a
        // hard error — distinct from the snapped zero-length case.
        let starts = vec![a("s1", 200)];
        let ends = vec![a("e1", 100)];
        let err = pair_vertical_lines(starts, ends, "blackout").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("reversed"), "msg = {msg}");
    }

    #[test]
    fn equal_times_drop_to_empty_not_error() {
        // A start and end snapped to the same timestamp (TradingView
        // bar-snapping on readback) is a zero-length window: it arms
        // nothing, so it's dropped rather than erroring. This is the
        // 2026-06-26 DELL fix — the old code hard-errored "reversed"
        // and aborted the whole arm.
        let starts = vec![a("s1", 100)];
        let ends = vec![a("e1", 100)];
        let pairs = pair_vertical_lines(starts, ends, "blackout").expect("ok");
        assert!(pairs.is_empty(), "zero-length window must be dropped");
    }

    #[test]
    fn touching_windows_pair_without_cross_pairing() {
        // Two back-to-back windows where the first's end coincides with
        // the second's start (8h-apart H1+ blackouts: A resumes exactly
        // as B pauses). Ends-before-starts at equal time keeps the
        // pairing aligned: (100,200) and (200,300), NOT a (200,200)
        // zero-length window stolen from the middle.
        let starts = vec![a("s_a", 100), a("s_b", 200)];
        let ends = vec![a("e_a", 200), a("e_b", 300)];
        let pairs = pair_vertical_lines(starts, ends, "blackout").expect("ok");
        assert_eq!(pairs.len(), 2);
        assert_eq!((pairs[0].0.time, pairs[0].1.time), (100, 200));
        assert_eq!((pairs[1].0.time, pairs[1].1.time), (200, 300));
    }

    #[test]
    fn collision_in_the_middle_drops_only_the_zero_length_window() {
        // The DELL failure shape: three events whose lines snapped so
        // that one window collapsed to zero length in the middle. The
        // healthy windows on either side must survive; only the
        // degenerate one is dropped.
        // Timeline: start@100, end@200 (good) ; start@200, end@200
        // (snapped, zero-length) ; start@300, end@400 (good).
        let starts = vec![a("s1", 100), a("s2", 200), a("s3", 300)];
        let ends = vec![a("e1", 200), a("e2", 200), a("e3", 400)];
        let pairs = pair_vertical_lines(starts, ends, "blackout").expect("ok");
        assert_eq!(pairs.len(), 2, "the zero-length middle window is dropped");
        assert_eq!((pairs[0].0.time, pairs[0].1.time), (100, 200));
        assert_eq!((pairs[1].0.time, pairs[1].1.time), (300, 400));
    }

    #[test]
    fn empty_inputs_are_ok() {
        let starts: Vec<Anchor> = vec![];
        let ends: Vec<Anchor> = vec![];
        let pairs = pair_vertical_lines(starts, ends, "blackout").expect("ok");
        assert!(pairs.is_empty());
    }
}
