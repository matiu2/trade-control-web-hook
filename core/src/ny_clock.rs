//! Hand-rolled US Eastern DST clock for the NY-close-edge detector.
//!
//! KV-free and clock-free (operates on a passed-in `DateTime<Utc>`), so
//! it's fully unit-testable. We hand-roll the rule rather than pull
//! `chrono-tz` (which bakes the whole IANA table into the WASM bundle).
//!
//! US rule: EDT (UTC−4) from the 2nd Sunday of March 02:00 local to the
//! 1st Sunday of November 02:00 local; EST (UTC−5) otherwise. NY equity
//! close is 17:00 local → 21:00 UTC under EDT, 22:00 UTC under EST.

use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc, Weekday};

/// UTC hour of the NY 17:00 close under EDT (UTC−4): 17 + 4 = 21.
const NY_CLOSE_HOUR_UTC_EDT: u32 = 21;
/// UTC hour of the NY 17:00 close under EST (UTC−5): 17 + 5 = 22.
const NY_CLOSE_HOUR_UTC_EST: u32 = 22;

/// Is `date` (a UTC calendar date) inside the US EDT window?
///
/// Approximation note: we key off the UTC *date*, which is correct
/// except inside the ~2h local-midnight-to-02:00 transition sliver — a
/// non-issue here because we only ever evaluate around 21:00–22:00 UTC.
pub fn ny_is_edt(date: NaiveDate) -> bool {
    let year = date.year();
    let Some(dst_start) = nth_weekday_of_month(year, 3, Weekday::Sun, 2) else {
        // 2nd Sunday of March always exists; the None branch is a
        // defensive fallback (treat as EST) for an impossible-date input.
        return false;
    };
    let Some(dst_end) = nth_weekday_of_month(year, 11, Weekday::Sun, 1) else {
        return false;
    };
    date >= dst_start && date < dst_end
}

/// True when `now` is at the NY-close edge: the UTC hour that equals
/// 17:00 America/New_York for the current season. 21:00 UTC under EDT,
/// 22:00 UTC under EST. We match on the *hour* (the daily cron fires at
/// :05 of the candidate hour) so a few minutes of jitter still lands.
pub fn is_ny_close_edge(now: DateTime<Utc>) -> bool {
    let close_hour_utc = if ny_is_edt(now.date_naive()) {
        NY_CLOSE_HOUR_UTC_EDT
    } else {
        NY_CLOSE_HOUR_UTC_EST
    };
    now.hour() == close_hour_utc
}

/// The date of the `n`-th `weekday` (1-based) in `(year, month)`.
///
/// Returns `None` only for an out-of-range `(year, month)` or when the
/// month has fewer than `n` of that weekday (which never happens for the
/// 1st/2nd Sunday queries this module makes). Pure, no clock feature.
fn nth_weekday_of_month(year: i32, month: u32, weekday: Weekday, n: u32) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    // Days to step from the 1st to the first occurrence of `weekday`.
    let offset = (7 + weekday.num_days_from_sunday() - first.weekday().num_days_from_sunday()) % 7;
    let day = offset + 1 + (n.checked_sub(1)?) * 7;
    NaiveDate::from_ymd_opt(year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    fn d(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid date fixture")
    }

    // --- the proven DST fixture table (mandatory) ---

    #[test]
    fn fixture_2026_03_05_est_close_2200_utc() {
        // 5-Mar-2026 is still EST (before the 2nd Sunday of March).
        let now = ts("2026-03-05T22:00:00Z");
        assert!(!ny_is_edt(now.date_naive()), "5-Mar is EST");
        assert!(is_ny_close_edge(now), "EST close edge is 22:00 UTC");
    }

    #[test]
    fn fixture_2026_03_12_edt_close_2100_utc() {
        // 12-Mar-2026 has crossed into EDT (2nd Sunday was 8-Mar).
        let now = ts("2026-03-12T21:00:00Z");
        assert!(ny_is_edt(now.date_naive()), "12-Mar is EDT");
        assert!(is_ny_close_edge(now), "EDT close edge is 21:00 UTC");
    }

    #[test]
    fn fixture_2026_04_02_edt_close_2100_utc() {
        let now = ts("2026-04-02T21:00:00Z");
        assert!(ny_is_edt(now.date_naive()));
        assert!(is_ny_close_edge(now));
    }

    #[test]
    fn fixture_2026_04_09_edt_close_2100_utc() {
        let now = ts("2026-04-09T21:00:00Z");
        assert!(ny_is_edt(now.date_naive()));
        assert!(is_ny_close_edge(now));
    }

    // --- wrong-hour / wrong-season negatives ---

    #[test]
    fn wrong_season_hour_is_not_edge() {
        // 12-Mar is EDT (edge 21:00) — 22:00 UTC must be false.
        let now = ts("2026-03-12T22:00:00Z");
        assert!(ny_is_edt(now.date_naive()));
        assert!(!is_ny_close_edge(now));
    }

    #[test]
    fn est_day_at_edt_hour_is_not_edge() {
        // 5-Mar is EST (edge 22:00) — 21:00 UTC must be false.
        let now = ts("2026-03-05T21:00:00Z");
        assert!(!ny_is_edt(now.date_naive()));
        assert!(!is_ny_close_edge(now));
    }

    #[test]
    fn unrelated_hour_is_not_edge() {
        let now = ts("2026-04-09T10:00:00Z");
        assert!(!is_ny_close_edge(now));
    }

    // --- DST-transition boundary exactness ---

    #[test]
    fn march_second_sunday_is_edt() {
        // 2026-03-08 is the 2nd Sunday of March → DST starts, EDT.
        assert!(ny_is_edt(d(2026, 3, 8)), "2nd Sunday of March is EDT");
        // 2026-03-07 (the Saturday before) is still EST.
        assert!(!ny_is_edt(d(2026, 3, 7)), "day before DST start is EST");
    }

    #[test]
    fn november_first_sunday_ends_dst() {
        // 2026-11-01 is the 1st Sunday of November → DST ends, EST.
        assert!(!ny_is_edt(d(2026, 11, 1)), "1st Sunday of November is EST");
        // 2026-10-31 (the Saturday before) is still EDT.
        assert!(ny_is_edt(d(2026, 10, 31)), "day before DST end is EDT");
    }

    // --- nth_weekday_of_month exactness ---

    #[test]
    fn nth_weekday_known_dates() {
        assert_eq!(
            nth_weekday_of_month(2026, 3, Weekday::Sun, 2),
            Some(d(2026, 3, 8)),
            "2nd Sunday of March 2026 is the 8th"
        );
        assert_eq!(
            nth_weekday_of_month(2026, 11, Weekday::Sun, 1),
            Some(d(2026, 11, 1)),
            "1st Sunday of November 2026 is the 1st"
        );
    }

    #[test]
    fn nth_weekday_first_when_month_starts_on_target() {
        // 2026-11-01 is itself a Sunday, so the 1st Sunday is day 1.
        assert_eq!(
            nth_weekday_of_month(2026, 11, Weekday::Sun, 1),
            Some(d(2026, 11, 1))
        );
        // and the 2nd Sunday is day 8.
        assert_eq!(
            nth_weekday_of_month(2026, 11, Weekday::Sun, 2),
            Some(d(2026, 11, 8))
        );
    }
}
