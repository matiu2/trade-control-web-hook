//! The `TimedAnchor` trait — a single-timestamp anchor abstraction over a
//! drawing.
//!
//! History: this module used to also pair drawn `start`/`end` vertical-line
//! markers (pause/resume, news-start/news-end) into `(start, end)` windows via
//! `pair_vertical_lines`. That pairing is gone — news and blackout windows now
//! come from the economic calendar at real event-minute precision (see
//! `tv-arm`'s `news_window` / `calendar_windows`), so nothing reads drawn lines
//! back off the chart. All that survives is the anchor trait, still used by
//! tv-arm's single-slot role pickers to read a drawing's anchor time.

/// Anything that carries a single timestamp anchor — vertical lines
/// have one point, so `anchor_time` is just that point's
/// time-in-seconds.
pub trait TimedAnchor {
    /// UNIX seconds of the line's anchor.
    fn anchor_time(&self) -> i64;
}
