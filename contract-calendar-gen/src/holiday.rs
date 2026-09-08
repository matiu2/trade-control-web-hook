//! US exchange holidays and business-day arithmetic.
//!
//! Every close-out deadline in this crate is expressed as "N business days
//! before X", so business-day counting is the primitive the whole calendar
//! rests on. A missed holiday shifts a deadline by a day in the *permissive*
//! direction (we would think we have one more day than we do), which is
//! exactly the error that gets a position force-liquidated. So the holiday
//! table is explicit and dated rather than derived from a rule engine.
//!
//! # Which holidays
//!
//! CME/COMEX observe the US federal holiday set plus Good Friday. Note that
//! Good Friday is **not** a federal holiday but *is* an exchange holiday, and
//! it moves with Easter — which is why the table is dated rather than
//! computed. Half-days (the day after Thanksgiving, Christmas Eve) are **not**
//! listed: the exchange is open, so they are business days, and treating them
//! as holidays would push deadlines later — the unsafe direction.
//!
//! # Coverage
//!
//! The table covers 2026–2028. [`is_covered`] reports whether a date falls in
//! the covered span; the generator refuses to emit rows that would need
//! arithmetic beyond it rather than silently counting weekends only.

use chrono::{Datelike, Duration, NaiveDate, Weekday};

/// First year the holiday table covers.
pub const FIRST_COVERED_YEAR: i32 = 2026;
/// Last year the holiday table covers.
pub const LAST_COVERED_YEAR: i32 = 2028;

/// CME/COMEX holidays, `(year, month, day)`, sorted.
///
/// Sources: CME Group holiday calendar; US federal holidays with the standard
/// Saturday→Friday / Sunday→Monday observation shift. Good Friday is included
/// (an exchange holiday, not a federal one).
const HOLIDAYS: &[(i32, u32, u32)] = &[
    // ---- 2026 ----
    (2026, 1, 1),   // New Year's Day
    (2026, 1, 19),  // MLK Day (3rd Monday Jan)
    (2026, 2, 16),  // Presidents Day (3rd Monday Feb)
    (2026, 4, 3),   // Good Friday
    (2026, 5, 25),  // Memorial Day (last Monday May)
    (2026, 6, 19),  // Juneteenth
    (2026, 7, 3),   // Independence Day observed (Jul 4 is a Saturday)
    (2026, 9, 7),   // Labor Day (1st Monday Sep)
    (2026, 11, 26), // Thanksgiving (4th Thursday Nov)
    (2026, 12, 25), // Christmas Day
    // ---- 2027 ----
    (2027, 1, 1),   // New Year's Day
    (2027, 1, 18),  // MLK Day
    (2027, 2, 15),  // Presidents Day
    (2027, 3, 26),  // Good Friday
    (2027, 5, 31),  // Memorial Day
    (2027, 6, 18),  // Juneteenth observed (Jun 19 is a Saturday)
    (2027, 7, 5),   // Independence Day observed (Jul 4 is a Sunday)
    (2027, 9, 6),   // Labor Day
    (2027, 11, 25), // Thanksgiving
    (2027, 12, 24), // Christmas Day observed (Dec 25 is a Saturday)
    // ---- 2028 ----
    (2028, 1, 17),  // MLK Day (Jan 1 is a Saturday; New Year observed Dec 31 2027)
    (2028, 2, 21),  // Presidents Day
    (2028, 4, 14),  // Good Friday
    (2028, 5, 29),  // Memorial Day
    (2028, 6, 19),  // Juneteenth
    (2028, 7, 4),   // Independence Day
    (2028, 9, 4),   // Labor Day
    (2028, 11, 23), // Thanksgiving
    (2028, 12, 25), // Christmas Day
];

/// Is `date` a CME/COMEX holiday?
pub fn is_holiday(date: NaiveDate) -> bool {
    HOLIDAYS
        .iter()
        .any(|&(y, m, d)| date.year() == y && date.month() == m && date.day() == d)
}

/// Is `date` a weekend?
pub fn is_weekend(date: NaiveDate) -> bool {
    matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
}

/// Is `date` a business day — a weekday that isn't an exchange holiday?
pub fn is_business_day(date: NaiveDate) -> bool {
    !is_weekend(date) && !is_holiday(date)
}

/// Does the holiday table cover `date`'s year?
///
/// Business-day arithmetic outside the covered span would count weekends only
/// and silently produce a deadline one or more days *later* than the truth —
/// the unsafe direction. Callers must refuse rather than extrapolate.
pub fn is_covered(date: NaiveDate) -> bool {
    (FIRST_COVERED_YEAR..=LAST_COVERED_YEAR).contains(&date.year())
}

/// Step back `n` business days from `date`.
///
/// `n = 0` returns `date` unchanged even if it isn't itself a business day —
/// the caller decides whether that matters. Each step skips weekends and
/// holidays. Returns `None` if the walk leaves the covered span, so an
/// out-of-range answer is a refusal rather than a wrong date.
pub fn business_days_before(date: NaiveDate, n: u32) -> Option<NaiveDate> {
    let mut cursor = date;
    for _ in 0..n {
        loop {
            cursor = cursor.checked_sub_signed(Duration::days(1))?;
            if !is_covered(cursor) {
                return None;
            }
            if is_business_day(cursor) {
                break;
            }
        }
    }
    Some(cursor)
}

/// The last business day of the month containing `date`.
pub fn last_business_day_of_month(year: i32, month: u32) -> Option<NaiveDate> {
    let mut cursor = last_day_of_month(year, month)?;
    while !is_business_day(cursor) {
        cursor = cursor.checked_sub_signed(Duration::days(1))?;
    }
    Some(cursor)
}

/// The `n`th-last business day of the given month (`n = 1` ⇒ the last).
///
/// COMEX gold's last trade day is the *third* last business day of the
/// delivery month, so this is the primitive that rule needs.
pub fn nth_last_business_day_of_month(year: i32, month: u32, n: u32) -> Option<NaiveDate> {
    let last = last_business_day_of_month(year, month)?;
    if n <= 1 {
        return Some(last);
    }
    business_days_before(last, n - 1)
}

/// The `n`th occurrence of `weekday` in the given month (`n = 1` ⇒ the first).
///
/// CME equity index futures stop trading on the *third Friday* of the contract
/// month, which is what this serves.
pub fn nth_weekday_of_month(year: i32, month: u32, weekday: Weekday, n: u32) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let offset = (7 + weekday.num_days_from_monday() - first.weekday().num_days_from_monday()) % 7;
    let day = 1 + offset + (n.checked_sub(1)? * 7);
    let candidate = NaiveDate::from_ymd_opt(year, month, day)?;
    (candidate.month() == month).then_some(candidate)
}

/// The last calendar day of the given month.
fn last_day_of_month(year: i32, month: u32) -> Option<NaiveDate> {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(next_year, next_month, 1)?.checked_sub_signed(Duration::days(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).expect("valid test date")
    }

    #[test]
    fn weekends_are_not_business_days() {
        // 2026-09-05 is a Saturday, 2026-09-06 a Sunday.
        assert_eq!(d(2026, 9, 5).weekday(), Weekday::Sat);
        assert!(!is_business_day(d(2026, 9, 5)));
        assert!(!is_business_day(d(2026, 9, 6)));
        assert!(is_business_day(d(2026, 9, 4)));
    }

    #[test]
    fn thanksgiving_is_a_holiday_and_shifts_the_count() {
        // Thanksgiving 2026 is Thu Nov 26. Two business days before Fri Nov 27
        // is Tue Nov 24, NOT Wed Nov 25 — the holiday is skipped.
        assert!(is_holiday(d(2026, 11, 26)));
        assert!(!is_business_day(d(2026, 11, 26)));
        assert_eq!(
            business_days_before(d(2026, 11, 27), 2),
            Some(d(2026, 11, 24)),
            "Thanksgiving must be skipped when counting back"
        );
    }

    #[test]
    fn good_friday_is_an_exchange_holiday() {
        // Good Friday is not a US federal holiday but the exchange is closed.
        assert!(is_holiday(d(2026, 4, 3)));
        assert!(!is_business_day(d(2026, 4, 3)));
        // One business day before Mon Apr 6 skips both the weekend and Good
        // Friday, landing on Thu Apr 2.
        assert_eq!(business_days_before(d(2026, 4, 6), 1), Some(d(2026, 4, 2)));
    }

    #[test]
    fn counting_back_crosses_a_weekend() {
        // Mon 2026-09-14 back one business day is Fri 2026-09-11.
        assert_eq!(
            business_days_before(d(2026, 9, 14), 1),
            Some(d(2026, 9, 11))
        );
    }

    #[test]
    fn zero_business_days_is_identity() {
        assert_eq!(business_days_before(d(2026, 9, 5), 0), Some(d(2026, 9, 5)));
    }

    #[test]
    fn last_business_day_skips_a_weekend_month_end() {
        // 2026-10-31 is a Saturday ⇒ last business day is Fri Oct 30.
        assert_eq!(d(2026, 10, 31).weekday(), Weekday::Sat);
        assert_eq!(last_business_day_of_month(2026, 10), Some(d(2026, 10, 30)));
    }

    #[test]
    fn third_last_business_day_of_december_2026() {
        // Dec 2026: 31st is a Thursday, 25th (Christmas) is a Friday holiday.
        // Business days at month end: ..., 28, 29, 30, 31 ⇒ 3rd last = Dec 29.
        assert_eq!(
            nth_last_business_day_of_month(2026, 12, 3),
            Some(d(2026, 12, 29))
        );
    }

    #[test]
    fn third_friday_of_september_2026() {
        // Verified against the live paper Gateway: ES/MES 202609 last trade
        // 2026-09-18, which is the third Friday.
        assert_eq!(
            nth_weekday_of_month(2026, 9, Weekday::Fri, 3),
            Some(d(2026, 9, 18))
        );
    }

    #[test]
    fn nth_weekday_returns_none_when_the_month_lacks_one() {
        // No month has five Fridays starting on the 1st in every case; asking
        // for the 6th must fail rather than roll into the next month.
        assert_eq!(nth_weekday_of_month(2026, 9, Weekday::Fri, 6), None);
    }

    #[test]
    fn uncovered_years_are_refused_not_extrapolated() {
        assert!(!is_covered(d(2025, 6, 1)));
        assert!(!is_covered(d(2029, 6, 1)));
        assert!(is_covered(d(2026, 6, 1)));
        // Walking out of the covered span refuses rather than guessing.
        assert_eq!(business_days_before(d(2026, 1, 2), 5), None);
    }

    #[test]
    fn observed_holidays_shift_off_the_weekend() {
        // Jul 4 2026 is a Saturday ⇒ observed Friday Jul 3.
        assert_eq!(d(2026, 7, 4).weekday(), Weekday::Sat);
        assert!(is_holiday(d(2026, 7, 3)));
        assert!(!is_holiday(d(2026, 7, 4)));
    }
}
