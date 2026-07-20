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

/// TEMPORARY FIX (2026-07-20): only treat a session gap as a real close→open
/// blackout when it is at least this long. The broker's `market_info` session
/// string is a day-of-week-BLIND typical-day clock (e.g. EUR/USD reports
/// `"00:00 - 22:00 & 22:05 - 23:59"` — a **5-minute** daily housekeeping gap),
/// so the old "any non-zero gap" rule inflated a meaningless blip into a
/// multi-hour DAILY no-entry window that (via the day-blind gate) rejected
/// legitimate mid-week FX entries. Empirically (the `tn_gap_scan` probe) the
/// only REAL trading gaps in the H1 price data are weekends (~49h) — there are
/// no genuine sub-24h daily closes for the instruments we trade.
///
/// **Important consequence of the current data model:** `windows_from_session`
/// derives windows from a *minute-of-day* clock string, so every gap it can see
/// is strictly `< 24h` (it wraps within a single day). A weekend gap is never
/// even visible here — the broker doesn't report it. Therefore a `>= 24h`
/// threshold means this function now emits **no windows at all** for a
/// day-blind session string. That is deliberate for now: it disables the
/// phantom daily blackout across the board (FX, gold, and — until the
/// candle-based rebuild — any real daily-gap index too), which is strictly
/// safer than blocking good trades. The real close detection moves to the
/// observed-gap (candle-derived) table, mirroring the spread-hour mask.
const MIN_REAL_GAP_MINUTES: u32 = 24 * 60;

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
        // TEMPORARY FIX: only a gap >= MIN_REAL_GAP_MINUTES (24h) counts as a
        // real close→open blackout. This drops the broker's phantom daily
        // housekeeping gaps (a zero-width abutting gap was already excluded by
        // the same test). See MIN_REAL_GAP_MINUTES — with today's minute-of-day
        // model this means no window is emitted, which safely disables the
        // day-blind blackout until the candle-derived table lands.
        if gap < MIN_REAL_GAP_MINUTES {
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

    /// The `>= 24h` temp-fix threshold makes `windows_from_session` unable to
    /// emit a window from any day-blind minute-of-day session (every such gap is
    /// `< 24h`). The real close→open protection now belongs to the candle-derived
    /// table; `merge_windows` is still tested directly below so its ring maths
    /// stays covered even though the deriver no longer feeds it.
    #[test]
    fn merge_windows_still_merges_overlapping_windows_on_the_ring() {
        // Two overlapping windows on the ring collapse to one contiguous block.
        let a = NoEntryWindow::new(19 * 60, 23 * 60); // 19:00–23:00
        let b = NoEntryWindow::new(22 * 60, 60); // 22:00–01:00 (wraps)
        let merged = merge_windows(vec![a, b]);
        assert_eq!(merged.len(), 1, "overlapping windows merge: {merged:?}");
        let w = merged[0];
        assert!(is_inside_window(19 * 60, &w), "start blocked");
        assert!(is_inside_window(0, &w), "wrap-past-midnight blocked");
        assert!(!is_inside_window(2 * 60, &w), "after the block allowed");
    }

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

    /// TEMPORARY FIX (>= 24h threshold): a sub-24h daily maintenance gap no
    /// longer produces a window. Wall Street 30's London
    /// `"00:00 - 22:00 & 23:00 - 23:59"` → Brisbane 10:00→08:00(+1d) and
    /// 09:00→09:59 has two closed gaps of 1h and 1m — both far below 24h, so
    /// under the new rule NO window is emitted. (Before the fix these buffered
    /// into one merged window.) This is the intended neutralisation: the real
    /// daily-gap detection moves to the candle-derived table.
    #[test]
    fn sub_24h_daily_gaps_emit_no_window_under_temp_fix() {
        let ranges = [r("10:00", "08:00 (+1d)"), r("09:00", "09:59")];
        let windows = windows_from_session(&ranges, Buffers::default());
        assert!(
            windows.is_empty(),
            "sub-24h daily gaps are dropped by the >=24h temp fix: {windows:?}"
        );
    }

    /// TEMPORARY FIX: a clean 4h overnight gap (the rolling-future shape the
    /// feature was originally built for) is ALSO dropped now — it is < 24h.
    /// Before the fix this produced one 17:00→01:00 UTC window (the exact shape
    /// that spuriously blocked the EUR/USD mid-week entry). Documented so the
    /// candle-based rebuild knows this legitimate daily-gap case is currently
    /// unprotected and must be restored by the observed-gap table.
    #[test]
    fn sub_24h_overnight_gap_emits_no_window_under_temp_fix() {
        // Brisbane: trades 10:00 → 06:00(+1d), closed 06:00 → 10:00 (4h gap).
        let ranges = [r("10:00", "06:00 (+1d)")];
        let windows = windows_from_session(&ranges, Buffers::default());
        assert!(
            windows.is_empty(),
            "a <24h overnight gap is dropped by the temp fix: {windows:?}"
        );
    }

    #[test]
    fn sub_24h_distant_gaps_emit_no_window_under_temp_fix() {
        // Two short (1h) gaps far apart. Both < 24h → no windows under the temp
        // fix (before: two separate windows).
        let ranges = [r("00:00", "11:00"), r("12:00", "23:00")];
        let windows = windows_from_session(&ranges, Buffers::default());
        assert!(
            windows.is_empty(),
            "sub-24h gaps produce nothing under the temp fix: {windows:?}"
        );
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
