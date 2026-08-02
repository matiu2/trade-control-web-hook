//! How far back to reach for the SL-floor's spread sample — in **bars of
//! market**, not hours of wall-clock.
//!
//! # The trap
//!
//! The entry SL floor sizes a stop off the mean spread over the last `window`
//! closed bars. To get those bars it asks the broker for a time range, and the
//! range used to be computed as `now − (window + 2) × bar_seconds`: pure
//! wall-clock arithmetic on the assumption that N bars occupy N × bar_seconds.
//!
//! **FX does not trade continuously.** The market closes Friday evening and
//! reopens Sunday evening, so a Monday-morning fire looking back 7 hours to
//! collect 5 H1 bars reaches into a **weekend** and finds two or three. Whatever
//! comes back is then meaned as though it were the intended sample:
//!
//! ```text
//! wanted: the last 5 bars     got: 3 bars, one of them Friday's NY-close spike
//! ```
//!
//! The floor is then sized off a sample that is both **too small** (so one spike
//! dominates it) and **discontinuous** (bars either side of a two-day gap are not
//! a trailing window of anything). Fewer samples is exactly the wrong direction:
//! the window exists *because* a single spiky bar shouldn't dominate.
//!
//! Observed on AUD/NZD 2026-06-11 entry #3, whose "last 5 H1 bars" spanned
//! 06-12T19:00Z → 06-14T23:00Z — two Friday bars, then a two-day hole, then
//! Sunday's reopen including the 18-pip NY-close bar.
//!
//! # The fix
//!
//! [`lookback_bars`] pads the count-back by a **calendar-aware** multiple rather
//! than a fixed `+2`, so the range is wide enough to still contain `window`
//! *traded* bars after a weekend has been carved out of it. Over-fetching is
//! free — [`crate::broker::trailing_spread_mean`] takes the **tail**, so extra
//! bars are discarded — whereas under-fetching silently shrinks the sample.
//!
//! This is the same wall-clock-vs-market-time confusion as the widen backstop in
//! [`super::widen_restore`], in a different place: there, 12h of wall-clock
//! spanned no market at all and fired a timer early. Both come from treating a
//! duration as if it measured trading activity.

/// Bars of slack added on top of `window` when the range cannot span a weekend.
///
/// The original `+2`: enough to absorb a partially-closed current bar and an
/// off-by-one at the range edge.
const CONTINUOUS_SLACK_BARS: i64 = 2;

/// The widest gap FX can present, in hours: Friday close → Sunday open is about
/// 48h, plus slack for brokers whose reopen drifts.
const MAX_MARKET_GAP_HOURS: i64 = 50;

/// How many bar-lengths back the SL-floor's candle fetch should reach to be sure
/// it still contains `window` **traded** bars.
///
/// Returns a count of bars, which the caller multiplies by the granularity's
/// seconds. The result deliberately **over**-reaches: `trailing_spread_mean`
/// keeps only the last `window`, so surplus bars cost one wider query and change
/// no decision, while a shortfall silently shrinks the sample the floor is sized
/// from.
///
/// `bar_seconds` must be positive; a non-positive value falls back to the
/// continuous-market padding rather than dividing by zero.
pub fn lookback_bars(window: u32, bar_seconds: i64) -> i64 {
    let window = window.max(1) as i64;
    if bar_seconds <= 0 {
        return window + CONTINUOUS_SLACK_BARS;
    }
    // Bars needed to step over the widest weekend at this granularity. On H1
    // that is ~50; on D1 it is ~3. Added whole, so the range spans the gap AND
    // still has `window` bars of market on the far side of it.
    let gap_seconds = MAX_MARKET_GAP_HOURS * 3600;
    let gap_bars = (gap_seconds + bar_seconds - 1) / bar_seconds; // ceil

    window + CONTINUOUS_SLACK_BARS + gap_bars
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reported window for AUD/NZD entry #3 spanned a weekend and returned
    /// bars either side of a two-day hole. At H1 the lookback must reach past a
    /// full weekend, so the fetch still contains 5 *traded* bars.
    #[test]
    fn an_h1_lookback_reaches_past_a_full_weekend() {
        let bars = lookback_bars(5, 3600);
        assert!(
            bars >= 5 + 48,
            "an H1 window must span a ~48h weekend and still hold 5 traded bars, got {bars}"
        );
    }

    /// The old behaviour, pinned as the thing that broke: 7 bars of H1 is ~7
    /// hours, which does not clear a weekend.
    #[test]
    fn the_old_fixed_slack_could_not_clear_a_weekend() {
        let old = 5 + CONTINUOUS_SLACK_BARS;
        let hours = old * 3600 / 3600;
        assert!(
            hours < 48,
            "precondition: the old +2 slack spans {hours}h, less than a weekend"
        );
        assert!(
            lookback_bars(5, 3600) > old,
            "the fix must reach further back than the old fixed slack"
        );
    }

    /// A coarse granularity needs few extra bars — a weekend is under 3 daily
    /// bars — so the padding scales with the timeframe instead of being constant.
    #[test]
    fn a_daily_lookback_adds_only_a_few_bars_for_the_same_gap() {
        let daily = lookback_bars(5, 86_400);
        let hourly = lookback_bars(5, 3600);
        assert!(
            daily < hourly,
            "the same 50h gap is fewer bars on D1 ({daily}) than H1 ({hourly})"
        );
        assert!(daily >= 5 + CONTINUOUS_SLACK_BARS + 3);
    }

    /// Always at least the window plus its original slack, so the fix can only
    /// ever fetch MORE than before — never fewer bars than the caller asked for.
    #[test]
    fn the_lookback_never_undercuts_the_window_itself() {
        for (w, secs) in [(1, 3600), (5, 3600), (20, 60), (5, 86_400)] {
            assert!(
                lookback_bars(w, secs) >= w as i64 + CONTINUOUS_SLACK_BARS,
                "window {w} at {secs}s undercut its own slack"
            );
        }
    }

    /// A degenerate bar length must not divide by zero; it falls back to the
    /// continuous padding.
    #[test]
    fn a_non_positive_bar_length_falls_back_instead_of_dividing_by_zero() {
        assert_eq!(lookback_bars(5, 0), 5 + CONTINUOUS_SLACK_BARS);
        assert_eq!(lookback_bars(5, -1), 5 + CONTINUOUS_SLACK_BARS);
    }

    /// A zero window is clamped to 1 rather than producing a range that can
    /// contain nothing.
    #[test]
    fn a_zero_window_is_clamped_to_one_bar() {
        assert_eq!(lookback_bars(0, 3600), lookback_bars(1, 3600));
    }
}
