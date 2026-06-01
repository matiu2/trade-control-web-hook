//! Chronologically pair `start` / `end` vertical-line markers into
//! `(start, end)` windows.
//!
//! Used for both blackout (pause/resume) and news-window
//! (news-start / news-end) lines. Hard-errors when:
//! - the start and end counts differ (orphan line on the chart), or
//! - any pair has `start.time >= end.time` (operator mislabeled the
//!   markers).
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

/// Pair `starts` and `ends` chronologically.
///
/// `kind` is the label used in error messages (`"blackout"` /
/// `"news"`) so the operator can find the misdrawn line on the
/// chart.
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
        if s.anchor_time() >= e.anchor_time() {
            return Err(eyre!(
                "{kind} pair #{} is reversed: start={} is at or after end={}. \
                 Each {kind}-start must precede its {kind}-end.",
                i + 1,
                s.anchor_time(),
                e.anchor_time(),
            ));
        }
        pairs.push((s, e));
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
        let starts = vec![a("s1", 200)];
        let ends = vec![a("e1", 100)];
        let err = pair_vertical_lines(starts, ends, "blackout").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("reversed"), "msg = {msg}");
    }

    #[test]
    fn errors_on_equal_times() {
        // Equal times still get rejected — a zero-length window can't
        // be meaningful.
        let starts = vec![a("s1", 100)];
        let ends = vec![a("e1", 100)];
        let err = pair_vertical_lines(starts, ends, "blackout").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("reversed"), "msg = {msg}");
    }

    #[test]
    fn empty_inputs_are_ok() {
        let starts: Vec<Anchor> = vec![];
        let ends: Vec<Anchor> = vec![];
        let pairs = pair_vertical_lines(starts, ends, "blackout").expect("ok");
        assert!(pairs.is_empty());
    }
}
