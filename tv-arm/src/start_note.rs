//! `--replay` without `--start`: derive the journaling replay cursor from a
//! chart **Note** labelled `start`.
//!
//! The journaling workflow (`tv-arm --replay`) treats one instant as "live
//! now" and walks the whole chart to find each role relative to it (see the
//! `--start` docs in `args.rs`). Typing that RFC3339 timestamp by hand is
//! tedious and error-prone, so an operator can instead drop a TradingView
//! **Note** (`text_note` kind) on the chart, write the single word `start`
//! in it, and point its anchor at the bar they want to treat as live-now.
//!
//! When `--replay` is set and `--start` is absent, [`resolve_start_from_note`]
//! finds that note and returns its **first anchor's time** (`points[0].time`,
//! UNIX seconds). That value then flows through the exact same `--start`
//! machinery — whole-chart discovery, prune cursor, effective arm time — so a
//! note-driven replay is identical to one where `--start <ts>` was typed.
//!
//! The operator's contract is "there should only be one" note saying `start`.
//! Two matching notes is therefore an error, not a silent latest-wins: an
//! ambiguous cursor would journal the wrong window without warning.

use color_eyre::eyre::{Result, eyre};
use trading_view::drawings::{Drawing, DrawingStub};

use crate::roles::DrawingFetcher;

/// tv-mcp's kind string for a TradingView Note drawing.
const TEXT_NOTE_KIND: &str = "text_note";

/// The label an operator writes in the note to mark the replay start.
const START_LABEL: &str = "start";

/// Resolve the replay-start cursor from a chart Note labelled `start`.
///
/// Filters `stubs` to `text_note` drawings, fetches each in full, and keeps
/// the ones whose trimmed/lower-cased label is exactly `start`. Returns:
/// - `Ok(Some(time))` — the first anchor (`points[0].time`) of the sole match.
/// - `Ok(None)` — no note says `start` (caller falls back to its own default).
/// - `Err(..)` — more than one note says `start` (ambiguous cursor), or the
///   single match carries no usable anchor.
///
/// Only the `text_note` stubs are fetched, so this is cheap even on a chart
/// crowded with rays / trend lines / fibs.
pub fn resolve_start_from_note<F: DrawingFetcher>(
    fetcher: &F,
    stubs: &[DrawingStub],
) -> Result<Option<i64>> {
    let mut notes = Vec::new();
    for stub in stubs {
        if stub.name != TEXT_NOTE_KIND {
            continue;
        }
        notes.push(fetcher.get_drawing(&stub.id)?);
    }
    pick_start_note(&notes)
}

/// Pure picker over already-fetched Note drawings — the testable core of
/// [`resolve_start_from_note`]. `notes` should already be the `text_note`
/// subset; a non-note that happens to be labelled `start` is not this
/// function's concern (the caller filters by kind first).
fn pick_start_note(notes: &[Drawing]) -> Result<Option<i64>> {
    let matches: Vec<&Drawing> = notes
        .iter()
        .filter(|d| d.label().eq_ignore_ascii_case(START_LABEL))
        .collect();

    match matches.as_slice() {
        [] => Ok(None),
        [only] => start_time(only),
        many => Err(eyre!(
            "found {} chart Notes saying `{START_LABEL}` — expected exactly one; \
             remove the extras so the replay cursor is unambiguous",
            many.len()
        )),
    }
}

/// The first anchor's time of a `start` note, erroring if it has no usable
/// anchor (a degenerate `null`-time readback or a point-less note).
fn start_time(note: &Drawing) -> Result<Option<i64>> {
    let point = note
        .points
        .first()
        .ok_or_else(|| eyre!("the `{START_LABEL}` Note has no anchor point to read a time from"))?;
    if point.time <= 0 {
        return Err(eyre!(
            "the `{START_LABEL}` Note's first anchor has no valid time (read back as null/zero)"
        ));
    }
    Ok(Some(point.time))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use trading_view::drawings::{Point, Properties};

    /// In-memory fetcher backed by id → Drawing, mirroring roles.rs's mock.
    struct MockFetcher {
        drawings: HashMap<String, Drawing>,
    }

    impl DrawingFetcher for MockFetcher {
        fn get_drawing(&self, entity_id: &str) -> Result<Drawing> {
            self.drawings
                .get(entity_id)
                .cloned()
                .ok_or_else(|| eyre!("unknown id {entity_id}"))
        }
    }

    fn note(id: &str, label: &str, points: Vec<(i64, f64)>) -> Drawing {
        Drawing {
            id: id.to_string(),
            points: points
                .into_iter()
                .map(|(time, price)| Point { time, price })
                .collect(),
            properties: Properties {
                text: Some(label.to_string()),
                ..Default::default()
            },
        }
    }

    fn stub(id: &str, name: &str) -> DrawingStub {
        DrawingStub {
            id: id.to_string(),
            name: name.to_string(),
        }
    }

    #[test]
    fn picks_first_anchor_time_of_the_sole_start_note() {
        // Two anchors — the FIRST one is the cursor (matches the live chart's
        // VdU92D note: points[0].time = 1783940400 = 13 Jul 9pm Brisbane).
        let notes = vec![note(
            "n1",
            "start",
            vec![(1783940400, 1.34026), (1783965600, 1.3472)],
        )];
        assert_eq!(pick_start_note(&notes).unwrap(), Some(1783940400));
    }

    #[test]
    fn label_match_is_trimmed_and_case_insensitive() {
        let notes = vec![note("n1", "  Start ", vec![(1700000000, 1.0)])];
        assert_eq!(pick_start_note(&notes).unwrap(), Some(1700000000));
    }

    #[test]
    fn no_start_note_returns_none() {
        // Other notes on the chart don't count — only the exact `start` label.
        let notes = vec![
            note("n1", "too-low", vec![(1700000000, 1.0)]),
            note("n2", "normal entry", vec![(1700003600, 1.1)]),
        ];
        assert_eq!(pick_start_note(&notes).unwrap(), None);
    }

    #[test]
    fn a_note_that_merely_contains_start_does_not_match() {
        // "there should only be one" word `start`; a longer sentence is not it.
        let notes = vec![note("n1", "start here maybe", vec![(1700000000, 1.0)])];
        assert_eq!(pick_start_note(&notes).unwrap(), None);
    }

    #[test]
    fn two_start_notes_is_an_error() {
        let notes = vec![
            note("n1", "start", vec![(1700000000, 1.0)]),
            note("n2", "start", vec![(1700003600, 1.1)]),
        ];
        let err = pick_start_note(&notes).unwrap_err().to_string();
        assert!(err.contains("expected exactly one"), "got: {err}");
    }

    #[test]
    fn start_note_without_anchor_is_an_error() {
        let notes = vec![note("n1", "start", vec![])];
        assert!(pick_start_note(&notes).is_err());
    }

    #[test]
    fn start_note_with_null_time_anchor_is_an_error() {
        // time == 0 is tv-mcp's sentinel for a `null` readback.
        let notes = vec![note("n1", "start", vec![(0, 1.0)])];
        assert!(pick_start_note(&notes).is_err());
    }

    #[test]
    fn resolver_only_fetches_text_notes_and_finds_start() {
        // A chart full of rays + one start note. The resolver must fetch only
        // the note (fetching a ray id would 404 in this mock) and find it.
        let mut drawings = HashMap::new();
        drawings.insert(
            "note1".to_string(),
            note("note1", "start", vec![(1783940400, 1.34026)]),
        );
        let fetcher = MockFetcher { drawings };
        let stubs = vec![
            stub("ray1", "ray"),
            stub("ray2", "ray"),
            stub("note1", "text_note"),
        ];
        assert_eq!(
            resolve_start_from_note(&fetcher, &stubs).unwrap(),
            Some(1783940400)
        );
    }

    #[test]
    fn resolver_returns_none_when_no_notes_present() {
        let fetcher = MockFetcher {
            drawings: HashMap::new(),
        };
        let stubs = vec![stub("ray1", "ray"), stub("tl1", "trend_line")];
        assert_eq!(resolve_start_from_note(&fetcher, &stubs).unwrap(), None);
    }
}
