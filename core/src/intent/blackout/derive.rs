//! Derive the set of UTC blackout [`NoEntryWindow`]s from a broker trade
//! session, with **zero timezone math in the worker**.
//!
//! # Why this is pure arithmetic, not `chrono_tz`
//!
//! The DST-correct work is already done upstream: `tradenation-api` converts
//! the broker's London session hours into **Brisbane** clock strings (UTC+10,
//! anchored to *today* via the real `Europe/London` rules), e.g.
//! `"09:00 (+1d)"`. Brisbane has **no DST** — it is a fixed `UTC+10` offset —
//! so converting a Brisbane minute-of-day to UTC is the single subtraction
//! `utc = (brisbane − 600) mod 1440`. That is the whole reason the (WASM) cron
//! never links `chrono_tz`: it consumes the already-seasoned Brisbane strings
//! and does modular arithmetic. See the `market-hours-blackout-design` memory.
//!
//! # What a "blackout window" is
//!
//! A market trades in one or more [`SessionRange`]s per day. The **gaps**
//! between consecutive ranges (wrapping midnight) are when it is closed — and a
//! resting order left across a close→open gap is exactly the incident this
//! feature fixes. For **every** such gap we emit a window
//! `[gap_start − before … gap_end + after]` (the [`Buffers`]), then **merge**
//! any windows that overlap after buffering into a minimal set.
//!
//! A market with no gaps (truly 24h, or a session string that didn't parse
//! into ranges) yields **no windows** — fail-open, never blocked. The
//! reduced-liquidity "spread hour" is a *separate* feature (the spread-blackout
//! cron) and is deliberately not covered here.

use super::{MINUTES_PER_DAY, NoEntryWindow};

/// Buffers applied around each close→open gap, in minutes. Entry is blocked
/// from `before_close` minutes *before* the market closes until `after_open`
/// minutes *after* it reopens, so a resting order never survives the gap and a
/// fresh one isn't placed right before/after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Buffers {
    /// Minutes before the close to start blocking (default 180 = 3h).
    pub before_close: u32,
    /// Minutes after the open to keep blocking (default 60 = 1h).
    pub after_open: u32,
}

impl Default for Buffers {
    fn default() -> Self {
        Self {
            before_close: 3 * 60,
            after_open: 60,
        }
    }
}

/// Brisbane is UTC+10 year-round — the fixed shift from a Brisbane
/// minute-of-day back to UTC.
const BRISBANE_OFFSET_MIN: u32 = 10 * 60;

/// Parse a Brisbane session clock string into a **UTC minute-of-day**.
///
/// Accepts the `tradenation-api` display form `"HH:MM"` optionally suffixed
/// with a day-rollover marker ` (+1d)` / ` (-1d)` — the suffix only told a
/// human which calendar day the Brisbane wall-clock landed on; for a
/// daily-recurring minute-of-day window it carries no extra information once we
/// reduce modulo a day, so we strip and ignore it. Returns `None` on any
/// malformed input (the caller drops that range rather than guessing).
fn brisbane_str_to_utc_min(s: &str) -> Option<u32> {
    // Drop the " (+1d)" / " (-1d)" rollover marker if present.
    let clock = s.split('(').next().unwrap_or(s).trim();
    let (h, m) = clock.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    let bne_min = h * 60 + m;
    // UTC = Brisbane − 10h, wrapped into the day.
    Some((bne_min + MINUTES_PER_DAY - BRISBANE_OFFSET_MIN) % MINUTES_PER_DAY)
}

/// A trading range as two UTC minute-of-day endpoints.
#[derive(Debug, Clone, Copy)]
struct RangeUtc {
    open: u32,
    close: u32,
}

/// Build the set of UTC blackout windows from a market's Brisbane session
/// ranges. Each tuple is `(open_brisbane, close_brisbane)` — exactly the
/// `SessionRange::open_brisbane` / `close_brisbane` fields. Ranges that fail to
/// parse are dropped (never block on garbage).
///
/// Returns an empty `Vec` when there is nothing to protect (no ranges, or a
/// single all-day range) — the fail-open case.
pub fn windows_from_session(ranges: &[(String, String)], buffers: Buffers) -> Vec<NoEntryWindow> {
    let utc: Vec<RangeUtc> = ranges
        .iter()
        .filter_map(|(open, close)| {
            Some(RangeUtc {
                open: brisbane_str_to_utc_min(open)?,
                close: brisbane_str_to_utc_min(close)?,
            })
        })
        .collect();

    // No parseable ranges → nothing to protect.
    if utc.is_empty() {
        return Vec::new();
    }

    // Sort ranges by their open minute so "consecutive" is well defined.
    let mut utc = utc;
    utc.sort_by_key(|r| r.open);

    // Each gap is the closed period between one range's close and the NEXT
    // range's open (the last range wraps round to the first). A buffered
    // blackout for that gap is `[gap_start − before … gap_end + after]`.
    let n = utc.len();
    let mut raw: Vec<NoEntryWindow> = Vec::new();
    for i in 0..n {
        let close = utc[i].close;
        let next_open = utc[(i + 1) % n].open;
        // Gap width (closed duration), wrapping midnight.
        let gap = (next_open + MINUTES_PER_DAY - close) % MINUTES_PER_DAY;
        // A zero-width gap (ranges abut exactly) is not a close→open gap.
        if gap == 0 {
            continue;
        }
        let open_min = (close + MINUTES_PER_DAY - buffers.before_close) % MINUTES_PER_DAY;
        let close_min = (next_open + buffers.after_open) % MINUTES_PER_DAY;
        raw.push(NoEntryWindow::new(open_min, close_min));
    }

    merge_windows(raw)
}

/// Merge overlapping / adjacent windows into a minimal set.
///
/// Windows live on a 24h **ring**, so a naive sweep-line on linear intervals is
/// wrong for the wrap case. We rasterise onto a 1440-minute ring bitmap (one
/// bit per minute, set across each window's `[open, close)` honouring the
/// wrap), then read the contiguous blocked runs back out as merged windows.
/// O(1440) per call — trivially cheap and unambiguous about the wrap.
///
/// If buffering ends up covering *every* minute (a market with no real trading
/// minutes left), there is no protectable open→close structure, so we return
/// empty — fail-open — rather than synthesise an all-day blackout.
fn merge_windows(windows: Vec<NoEntryWindow>) -> Vec<NoEntryWindow> {
    if windows.is_empty() {
        return Vec::new();
    }
    let mut blocked = [false; MINUTES_PER_DAY as usize];
    for w in &windows {
        let mut m = w.open_min;
        // Walk [open, close) on the ring. A degenerate open==close window
        // contributes nothing (matches `is_inside_window`).
        while m != w.close_min {
            blocked[m as usize] = true;
            m = (m + 1) % MINUTES_PER_DAY;
        }
    }

    // Whole day blocked → no protectable open→close structure; fail open.
    if blocked.iter().all(|b| *b) {
        return Vec::new();
    }

    // Find a minute that is NOT blocked to use as a ring "cut point", so each
    // blocked run is a contiguous linear segment from there. The all-blocked
    // case returned above, so an unblocked minute always exists; fall back to 0
    // rather than panic if that invariant ever changed.
    let start = blocked.iter().position(|b| !*b).unwrap_or(0) as u32;

    let mut out = Vec::new();
    let mut i = 0u32;
    while i < MINUTES_PER_DAY {
        let m = (start + i) % MINUTES_PER_DAY;
        if blocked[m as usize] {
            let open = m;
            // Extend over the contiguous blocked run.
            let mut len = 0u32;
            while len < MINUTES_PER_DAY {
                let mm = (start + i + len) % MINUTES_PER_DAY;
                if !blocked[mm as usize] {
                    break;
                }
                len += 1;
            }
            let close = (open + len) % MINUTES_PER_DAY;
            out.push(NoEntryWindow::new(open, close));
            i += len;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::is_inside_window;
    use super::*;

    fn r(open: &str, close: &str) -> (String, String) {
        (open.to_string(), close.to_string())
    }

    #[test]
    fn brisbane_to_utc_subtracts_ten_hours() {
        // 10:00 Brisbane = 00:00 UTC.
        assert_eq!(brisbane_str_to_utc_min("10:00"), Some(0));
        // 09:00 Brisbane = 23:00 UTC (previous day, wraps).
        assert_eq!(brisbane_str_to_utc_min("09:00"), Some(23 * 60));
        // 00:00 Brisbane = 14:00 UTC.
        assert_eq!(brisbane_str_to_utc_min("00:00"), Some(14 * 60));
    }

    #[test]
    fn brisbane_to_utc_ignores_day_rollover_marker() {
        // The "(+1d)" marker is human display only; minute-of-day is the same.
        assert_eq!(
            brisbane_str_to_utc_min("07:00 (+1d)"),
            brisbane_str_to_utc_min("07:00")
        );
        assert_eq!(brisbane_str_to_utc_min("09:00 (+1d)"), Some(23 * 60));
    }

    #[test]
    fn brisbane_to_utc_rejects_garbage() {
        assert_eq!(brisbane_str_to_utc_min("not a time"), None);
        assert_eq!(brisbane_str_to_utc_min("25:00"), None);
        assert_eq!(brisbane_str_to_utc_min("12:99"), None);
        assert_eq!(brisbane_str_to_utc_min(""), None);
    }

    #[test]
    fn no_ranges_is_fail_open() {
        assert!(windows_from_session(&[], Buffers::default()).is_empty());
    }

    #[test]
    fn single_all_day_range_has_no_gap() {
        // A 24h market: open == close (the range spans the whole day). The
        // single "gap" is zero-width → no window.
        let ranges = [r("00:00", "00:00")];
        assert!(windows_from_session(&ranges, Buffers::default()).is_empty());
    }

    #[test]
    fn unparseable_ranges_are_dropped_fail_open() {
        let ranges = [r("garbage", "also garbage")];
        assert!(windows_from_session(&ranges, Buffers::default()).is_empty());
    }

    /// Wall Street 30: London "00:00 - 22:00 & 23:00 - 23:59" → Brisbane the
    /// crate gives (winter/GMT example) roughly open/close pairs; we model the
    /// two ranges directly in Brisbane to assert the gap+buffer+merge maths.
    ///
    /// Brisbane ranges (GMT season, +10h): 10:00→08:00(+1d) and 09:00→09:59.
    /// The closed gaps are 08:00→09:00 (1h) and 09:59→10:00 (1m). With 3h/1h
    /// buffers the two buffered windows overlap and must MERGE into one.
    #[test]
    fn wall_street_two_gaps_merge_into_one() {
        let ranges = [r("10:00", "08:00 (+1d)"), r("09:00", "09:59")];
        let windows = windows_from_session(&ranges, Buffers::default());
        // The two near-adjacent gaps' buffered windows overlap → one window.
        assert_eq!(
            windows.len(),
            1,
            "overlapping windows must merge: {windows:?}"
        );

        // Convert the Brisbane anchors to the UTC minutes the window uses.
        // Brisbane 08:00 close → UTC 22:00; minus 3h buffer → UTC 19:00 open.
        // Brisbane 10:00 open  → UTC 00:00; plus 1h buffer  → UTC 01:00 close,
        // but the second gap (09:59→10:00) pushes the merged close to
        // UTC 00:00+1h = 01:00 as well. Assert the merged window blocks the
        // whole closed span and reopens after the buffer.
        let w = windows[0];
        // 19:00 UTC (3h before the 22:00 close) is blocked.
        assert!(is_inside_window(19 * 60, &w), "3h-before-close blocked");
        // 23:00 UTC (mid gap) blocked.
        assert!(is_inside_window(23 * 60, &w), "mid-gap blocked");
        // 00:30 UTC (just after reopen, inside +1h buffer) blocked.
        assert!(is_inside_window(30, &w), "inside after-open buffer blocked");
        // 18:00 UTC (before the buffer) allowed.
        assert!(!is_inside_window(18 * 60, &w), "before buffer allowed");
        // 12:00 UTC (well inside the trading session) allowed.
        assert!(!is_inside_window(12 * 60, &w), "mid-session allowed");
    }

    /// A clean single overnight close→open gap (no maintenance gap), the
    /// rolling-future shape. London index closes ~22:00, reopens ~23:00 — but
    /// model a wider, unambiguous gap in Brisbane to test one window end-to-end.
    #[test]
    fn single_overnight_gap_one_buffered_window() {
        // Brisbane: trades 10:00 → 06:00(+1d), closed 06:00 → 10:00 (4h gap).
        let ranges = [r("10:00", "06:00 (+1d)")];
        let windows = windows_from_session(&ranges, Buffers::default());
        assert_eq!(windows.len(), 1, "{windows:?}");
        let w = windows[0];
        // Brisbane close 06:00 → UTC 20:00; − 3h → 17:00 open.
        // Brisbane open 10:00 → UTC 00:00; + 1h → 01:00 close.
        assert_eq!(w.open_min, 17 * 60);
        assert_eq!(w.close_min, 60);
        assert!(w.wraps_midnight());
        assert!(is_inside_window(17 * 60, &w));
        assert!(is_inside_window(0, &w)); // midnight UTC, mid-gap
        assert!(!is_inside_window(60, &w)); // 01:00 UTC, reopened
        assert!(!is_inside_window(16 * 60 + 59, &w)); // before buffer
    }

    #[test]
    fn two_distant_gaps_stay_separate() {
        // Two trading sessions with SHORT gaps far apart so the 3h/1h buffers
        // can't bridge them. Brisbane: 00:00→11:00 and 12:00→23:00. Gaps are
        // 11:00→12:00 (1h) and 23:00→00:00 (1h), ~12h apart → TWO windows.
        let ranges = [r("00:00", "11:00"), r("12:00", "23:00")];
        let windows = windows_from_session(&ranges, Buffers::default());
        assert_eq!(windows.len(), 2, "distant gaps stay separate: {windows:?}");
    }

    #[test]
    fn buffers_filling_the_day_fail_open() {
        // Two 8h gaps with 3h+1h buffers each exactly tile the 24h ring (the
        // earlier over-eager test case): the whole day is blocked, which has no
        // protectable open→close structure → fail-open empty, never an all-day
        // blackout.
        let ranges = [r("00:00", "04:00"), r("12:00", "16:00")];
        let windows = windows_from_session(&ranges, Buffers::default());
        assert!(
            windows.is_empty(),
            "whole-day coverage fails open: {windows:?}"
        );
    }
}
