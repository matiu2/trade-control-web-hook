//! Read a fixed stop-loss price off a chart **Note** labelled `sl`.
//!
//! The operator drops a TradingView Note (tv-mcp kind `text_note`) with its
//! **first anchor** at the price they want the stop — typically the shoulder or
//! the head — and the enter uses that price instead of the geometry-anchored
//! default. This is the price-axis twin of [`crate::start_note`], which reads
//! `points[0].time` off a Note labelled `start`; this one reads
//! `points[0].price`.
//!
//! **Why `points[0]` and not some average.** A Note carries *two* anchors: the
//! point the operator placed, and the opposite corner of its text box. Only the
//! first is meaningful — the second drifts with the box's size and the chart's
//! zoom (confirmed on the live chart: a note anchored at `2.29965` reported a
//! second anchor of `2.295942460932459`, a number nobody chose). Reading
//! anything but `points[0]` would silently shift a real stop.
//!
//! ## Which Note counts: the time window
//!
//! A chart accumulates Notes — from earlier setups, from commentary. Picking
//! "the latest" or "the only one" would let a stale note from a previous trade
//! arm a stop at a price belonging to a different pattern, which is exactly the
//! failure mode [`crate::roles`]' visible-window filter exists to prevent for
//! every other role.
//!
//! So an `sl` Note only counts when its anchor sits inside
//! `[fib_earliest − SL_NOTE_LEAD_BARS × bar, trade_expiry]` — the setup's own
//! span, with a small lead so a note placed just left of the fib (at a shoulder
//! that predates it) still qualifies. Outside that window it is ignored and
//! logged, not an error: it's someone else's note.
//!
//! Inside the window the contract is strict, mirroring the `start` note: **zero
//! notes** means "no drawn stop, use the geometry default" (the feature is
//! opt-in and its absence must stay byte-identical to before), and **two or
//! more** is an error rather than a latest-wins guess — an ambiguous stop is
//! not something to resolve by coin-flip.

use color_eyre::eyre::{Result, eyre};
use tracing::{debug, info};
use trade_control_conventions::{SL_LABELS, matches};
use trading_view::drawings::{Drawing, DrawingStub};

use crate::roles::DrawingFetcher;

/// tv-mcp's kind string for a TradingView Note drawing. Same tool the
/// `start` note uses — see [`crate::start_note`].
const TEXT_NOTE_KIND: &str = "text_note";

/// How many bars *before* the fib's earliest anchor an `sl` Note may still
/// sit and count for this setup.
///
/// A stop is drawn at the shoulder or the head, and either can predate the fib
/// the operator drew over the pattern — the fib spans head→neckline, while the
/// shoulder sits to its left. A few bars of lead covers that without widening
/// the window far enough to catch the previous setup's leftovers.
pub const SL_NOTE_LEAD_BARS: i64 = 5;

/// Default ATR-percent buffer pushing a drawn `sl` stop clear of the level the
/// Note names.
///
/// The operator places the Note *at* the shoulder or head, so the stop belongs a
/// little past that wick rather than exactly on it — resting exactly on a wick
/// is the stop most likely to be clipped by the noise that formed the wick in
/// the first place. Matches `DEFAULT_BUFFER_ATR_PCT`, the same fraction the
/// pattern-anchored SL already uses, so a drawn stop and an anchored one sit the
/// same distance clear of their level.
///
/// Resolved against the live ATR at fire time, never baked (see
/// [`trade_control_core::intent::PriceRef::AbsoluteBuffered`]). Override
/// per-arm with `--sl-note-buffer-atr-pct`; `0` opts out and uses the drawn
/// price verbatim.
pub const DEFAULT_SL_NOTE_BUFFER_ATR_PCT: f64 = 0.5;

/// The time window an `sl` Note must be anchored inside to count for this
/// setup: `[fib_earliest − SL_NOTE_LEAD_BARS × bar_seconds, trade_expiry]`.
///
/// Returns `None` when either bound is unavailable (no fib, no trade-expiry, or
/// an unknown bar size). A window that can't be computed means the note can't
/// be *scoped*, and an unscoped note is exactly the stale-drawing hazard this
/// module exists to avoid — so the caller declines to read one at all rather
/// than falling back to an unbounded search.
pub fn sl_note_window(
    fib_earliest: Option<i64>,
    trade_expiry: Option<i64>,
    bar_seconds: i64,
) -> Option<(i64, i64)> {
    let (earliest, expiry) = (fib_earliest?, trade_expiry?);
    if bar_seconds <= 0 {
        return None;
    }
    let from = earliest.saturating_sub(SL_NOTE_LEAD_BARS.saturating_mul(bar_seconds));
    // A trade-expiry at or before the window start is a nonsense window (a
    // stale expiry line from an older setup). Decline rather than invert it.
    if expiry <= from {
        return None;
    }
    Some((from, expiry))
}

/// Resolve the drawn stop-loss price from a chart Note labelled `sl`.
///
/// Fetches only the `text_note` stubs (cheap even on a chart crowded with
/// trend lines), keeps those whose whole label is an [`SL_LABELS`] entry and
/// whose first anchor falls inside `window`, and returns:
///
/// - `Ok(Some(price))` — the sole in-window note's `points[0].price`.
/// - `Ok(None)` — no `sl` note in the window; caller keeps the geometry-anchored SL.
/// - `Err(..)` — more than one in-window note (ambiguous stop), or the sole
///   match carries no usable anchor.
pub fn resolve_sl_from_note<F: DrawingFetcher>(
    fetcher: &F,
    stubs: &[DrawingStub],
    window: (i64, i64),
) -> Result<Option<f64>> {
    let mut notes = Vec::new();
    for stub in stubs {
        if stub.name != TEXT_NOTE_KIND {
            continue;
        }
        notes.push(fetcher.get_drawing(&stub.id)?);
    }
    pick_sl_note(&notes, window)
}

/// Pure picker over already-fetched Note drawings — the testable core of
/// [`resolve_sl_from_note`]. `notes` should already be the `text_note` subset.
pub(crate) fn pick_sl_note(notes: &[Drawing], (from, to): (i64, i64)) -> Result<Option<f64>> {
    let labelled: Vec<&Drawing> = notes
        .iter()
        .filter(|d| matches(d.label(), SL_LABELS))
        .collect();

    // Scope to the setup's own window. A note outside it belongs to another
    // trade (or is commentary parked off to the side) — ignore it, but say so:
    // an operator who drew a stop and got the geometry default deserves a line
    // in the log explaining why.
    let (in_window, out): (Vec<&Drawing>, Vec<&Drawing>) = labelled
        .into_iter()
        .partition(|d| anchor_time(d).is_some_and(|t| t >= from && t <= to));
    if !out.is_empty() {
        info!(
            dropped_out_of_window = out.len(),
            from, to, "ignored `sl` Note(s) anchored outside this setup's window"
        );
    }

    match in_window.as_slice() {
        [] => {
            debug!(
                from,
                to, "no `sl` Note in window; SL stays geometry-anchored"
            );
            Ok(None)
        }
        [only] => sl_price(only).map(Some),
        many => Err(eyre!(
            "found {} chart Notes saying `sl` inside this setup's window — expected \
             exactly one; remove the extras so the stop-loss is unambiguous",
            many.len()
        )),
    }
}

/// The first anchor's time, or `None` when the note has no usable anchor.
fn anchor_time(note: &Drawing) -> Option<i64> {
    note.points.first().filter(|p| p.time > 0).map(|p| p.time)
}

/// The first anchor's price, erroring if the note has no usable anchor (a
/// degenerate `null` readback or a point-less note). Loud, because a note the
/// operator *did* draw failing to produce a stop must not pass silently.
fn sl_price(note: &Drawing) -> Result<f64> {
    let point = note
        .points
        .first()
        .ok_or_else(|| eyre!("the `sl` Note has no anchor point to read a price from"))?;
    if !point.price.is_finite() {
        return Err(eyre!(
            "the `sl` Note's first anchor has no valid price (read back as null)"
        ));
    }
    Ok(point.price)
}

#[cfg(test)]
mod tests {
    use super::*;
    use trading_view::drawings::{Point, Properties};

    /// H1 bars, the timeframe these setups are usually read on.
    const BAR: i64 = 3600;

    fn note(label: &str, points: Vec<(i64, f64)>) -> Drawing {
        Drawing {
            id: "n1".to_string(),
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

    /// A window spanning 1000..2000 in bar-seconds terms.
    fn window() -> (i64, i64) {
        (100_000, 200_000)
    }

    #[test]
    fn reads_the_first_anchor_price_of_the_sole_sl_note() {
        // Two anchors: the operator's placed point, then the text box's other
        // corner. Only the FIRST is the stop — the second is an artefact of
        // the box's size (see module docs).
        let notes = vec![note("sl", vec![(150_000, 2.30380), (160_000, 2.29594)])];
        assert_eq!(pick_sl_note(&notes, window()).unwrap(), Some(2.30380));
    }

    #[test]
    fn accepts_the_stop_loss_alias_and_odd_casing() {
        let notes = vec![note("  Stop-Loss  ", vec![(150_000, 1.2345)])];
        assert_eq!(pick_sl_note(&notes, window()).unwrap(), Some(1.2345));
    }

    #[test]
    fn no_sl_note_means_geometry_anchored_sl() {
        // The feature is opt-in: a chart with no `sl` note must behave exactly
        // as it did before this feature existed.
        let notes = vec![note("start", vec![(150_000, 2.30380)])];
        assert_eq!(pick_sl_note(&notes, window()).unwrap(), None);
    }

    #[test]
    fn commentary_mentioning_sl_is_not_a_stop() {
        // Whole-label match. A Note is free-form text and charts carry plenty
        // of it; only an exact `sl` / `stop-loss` is load-bearing.
        let notes = vec![
            note("sl too tight, moved it", vec![(150_000, 2.30380)]),
            note("v2 entry\ncontinuation", vec![(150_000, 2.29000)]),
        ];
        assert_eq!(pick_sl_note(&notes, window()).unwrap(), None);
    }

    /// The core of the window rule: a stale `sl` note from an earlier setup
    /// must not arm a stop on *this* one.
    #[test]
    fn sl_note_before_the_window_is_ignored() {
        let notes = vec![note("sl", vec![(50_000, 9.9999)])];
        assert_eq!(pick_sl_note(&notes, window()).unwrap(), None);
    }

    #[test]
    fn sl_note_after_trade_expiry_is_ignored() {
        let notes = vec![note("sl", vec![(250_000, 9.9999)])];
        assert_eq!(pick_sl_note(&notes, window()).unwrap(), None);
    }

    #[test]
    fn window_bounds_are_inclusive() {
        // A note drawn exactly on the lead edge or exactly at expiry counts —
        // the operator aimed at a bar, not at an open interval.
        for t in [100_000, 200_000] {
            let notes = vec![note("sl", vec![(t, 1.5)])];
            assert_eq!(
                pick_sl_note(&notes, window()).unwrap(),
                Some(1.5),
                "anchor at {t} should be in-window"
            );
        }
    }

    #[test]
    fn an_out_of_window_note_does_not_rescue_an_in_window_one() {
        // Two notes, one stale and one live: the live one wins outright, and
        // the pair must NOT read as ambiguous.
        let notes = vec![
            note("sl", vec![(50_000, 9.9999)]),
            note("sl", vec![(150_000, 2.30380)]),
        ];
        assert_eq!(pick_sl_note(&notes, window()).unwrap(), Some(2.30380));
    }

    #[test]
    fn two_in_window_sl_notes_are_ambiguous() {
        // Never a latest-wins guess — a stop is too consequential to pick by
        // draw order.
        let notes = vec![
            note("sl", vec![(150_000, 2.30380)]),
            note("sl", vec![(160_000, 2.31000)]),
        ];
        let err = pick_sl_note(&notes, window()).unwrap_err().to_string();
        assert!(err.contains("found 2"), "{err}");
        assert!(err.contains("exactly one"), "{err}");
    }

    #[test]
    fn a_degenerate_anchor_price_is_an_error_not_a_silent_skip() {
        // The operator drew a stop; failing to read it must be loud.
        let notes = vec![note("sl", vec![(150_000, f64::NAN)])];
        assert!(pick_sl_note(&notes, window()).is_err());
    }

    #[test]
    fn a_note_with_no_anchor_at_all_is_ignored_as_out_of_window() {
        // No anchor → no time → can't be scoped to the setup. Treated as
        // out-of-window rather than an error: an anchorless note isn't
        // demonstrably *this* setup's stop.
        let notes = vec![note("sl", vec![])];
        assert_eq!(pick_sl_note(&notes, window()).unwrap(), None);
    }

    // ---- the window itself ------------------------------------------------

    #[test]
    fn window_starts_five_bars_before_the_fib() {
        // The shoulder a stop is drawn at can predate the head→neckline fib.
        let got = sl_note_window(Some(100_000), Some(200_000), BAR).unwrap();
        assert_eq!(got, (100_000 - 5 * BAR, 200_000));
    }

    #[test]
    fn window_needs_both_bounds() {
        assert_eq!(sl_note_window(None, Some(200_000), BAR), None);
        assert_eq!(sl_note_window(Some(100_000), None, BAR), None);
    }

    #[test]
    fn window_needs_a_known_bar_size() {
        // An unknown granularity can't size the lead, and an unscoped note is
        // the very hazard this module guards against.
        assert_eq!(sl_note_window(Some(100_000), Some(200_000), 0), None);
    }

    #[test]
    fn window_declines_an_expiry_that_precedes_the_fib() {
        // A stale trade-expiry line left of the pattern would invert the
        // window; decline rather than search a backwards range.
        assert_eq!(sl_note_window(Some(100_000), Some(50_000), BAR), None);
    }

    #[test]
    fn window_scales_the_lead_with_the_timeframe() {
        // Five *bars*, not five hours — an H4 chart gets a proportionally
        // wider lead than an H1 one.
        let h1 = sl_note_window(Some(100_000), Some(200_000), 3600).unwrap();
        let h4 = sl_note_window(Some(100_000), Some(200_000), 4 * 3600).unwrap();
        assert!(
            h4.0 < h1.0,
            "H4 lead should reach further back: {h4:?} vs {h1:?}"
        );
    }
}
