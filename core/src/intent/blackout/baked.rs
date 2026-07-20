//! The candle-derived, weekday-aware market-hours blackout: a baked
//! `(venue, symbol) → daily-close overlay` table plus the universal weekend
//! rule, resolved into a [`WeekMask`](super::WeekMask) per instrument.
//!
//! # What this replaces
//!
//! The broker-session-string deriver ([`super::windows_from_session`]) inflated
//! a day-blind minute-of-day window from each broker's `market_info` string —
//! which turned a 5-minute daily housekeeping gap into a phantom multi-hour
//! blackout applied *every weekday*, wrongly rejecting mid-week entries. This
//! module reads the ACTUAL reopen price gaps measured off candles by
//! `market-hours-gen`, and separates the universal **weekend** halt from a
//! genuine per-instrument **mid-week daily close**. See the
//! `market-hours-blackout-weekly-gap-bug` memory.
//!
//! # The two rules
//!
//! - **Weekend** (universal): every instrument halts Friday evening → Sunday
//!   reopen. Baked as [`WEEKEND_*`] constants and applied to *every* mask.
//! - **Daily close** (per-instrument overlay): only instruments whose mid-week
//!   (Mon–Thu) reopen gaps clear the ATR-gap threshold get an extra daily block
//!   at their measured close hours. Carried in [`MARKET_HOURS_BAKED`].
//!
//! # Symbol keying
//!
//! Like the spread-hour table, the two venues' symbol namespaces don't collide
//! (`EUR/USD` TN vs `EUR_USD` OANDA), so the lookup keys on the symbol string
//! alone and the `venue` column is carried only for provenance/dedup.

use chrono::{DateTime, Utc};

use super::WeekMask;

/// Weekday of the Friday weekend-close, days-from-Monday (Fri = 4).
const WEEKEND_FROM_WEEKDAY: u32 = 4;
/// UTC minute-of-day the weekend halt begins. FX/metals/crypto reopen-gaps
/// cluster at Fri 20:00–21:00 UTC; we begin the block at 21:00 UTC — the
/// dominant Friday close hour — so the resting order is off before the halt.
const WEEKEND_FROM_MIN: u32 = 21 * 60;
/// Weekday the weekend halt ends, days-from-Monday (Sun = 6).
const WEEKEND_TO_WEEKDAY: u32 = 6;
/// UTC minute-of-day entry resumes on Sunday — the FX week reopens ~21:00–22:00
/// UTC Sunday (Monday morning Sydney/Tokyo). We reopen at 22:00 UTC.
const WEEKEND_TO_MIN: u32 = 22 * 60;

/// The candle-derived per-venue market-hours overlay produced offline by
/// `market-hours-gen` and committed as a source file. Row shape
/// `(venue, symbol, reviewed, daily_close_hours[6])`, sorted by `(venue,
/// symbol)`. See the module docs; the generator's `render.rs` documents each
/// column. The weekend block is NOT in the table (it's universal — see
/// [`weekend_mask`]).
#[allow(clippy::type_complexity)]
mod baked_table {
    include!("../../market_hours_baked.rs");
}
use baked_table::MARKET_HOURS_BAKED;

/// A mask carrying only the universal weekend block — the base every
/// instrument gets, before any per-instrument daily-close overlay.
fn weekend_mask() -> WeekMask {
    let mut m = WeekMask::empty();
    m.block_span(
        WEEKEND_FROM_WEEKDAY,
        WEEKEND_FROM_MIN,
        WEEKEND_TO_WEEKDAY,
        WEEKEND_TO_MIN,
    );
    m
}

/// Find the baked daily-close-hours overlay for a broker-native `symbol`, if the
/// table has a reviewed row for it. Scans both venues (symbol namespaces don't
/// collide). Returns the row's `daily_close_hours` with the `u32::MAX`
/// sentinels stripped; an empty slice ⇒ weekend-only.
fn baked_daily_hours(symbol: &str) -> Option<Vec<u32>> {
    let (_, _, reviewed, hours) = MARKET_HOURS_BAKED
        .iter()
        .find(|(_, sym, _, _)| *sym == symbol)?;
    // An unreviewed row (too few samples) is treated as weekend-only: return an
    // empty overlay rather than `None` so the caller still gets the weekend mask.
    if !reviewed {
        return Some(Vec::new());
    }
    Some(hours.iter().copied().filter(|h| *h != u32::MAX).collect())
}

/// The full weekday-aware blackout mask for `symbol`: the universal weekend
/// block, plus (if the instrument cleared the daily-close threshold) a daily
/// block spanning each measured close hour `[h .. h+1h)`.
///
/// Returns `None` **only** when the symbol isn't in the baked table at all — an
/// uncatalogued instrument, for which the caller falls open (no blackout). Every
/// catalogued instrument gets at least the weekend mask.
pub fn baked_market_hours(symbol: &str) -> Option<WeekMask> {
    let hours = baked_daily_hours(symbol)?;
    let mut mask = weekend_mask();
    for h in hours {
        // Block the close hour itself: `[h:00 .. (h+1):00)`. The reopen gap
        // follows this close, and the resting order must be off before it.
        mask.block_daily(h * 60, (h + 1) * 60);
    }
    Some(mask)
}

/// Is entry blocked for `symbol` at this UTC instant, per the baked market-hours
/// mask? `false` when the symbol isn't catalogued (fail open) or the instant is
/// outside every blocked span. This is the single predicate the reject gate and
/// sweep call — no KV read, no timezone math.
pub fn market_hours_blocked(symbol: &str, now: DateTime<Utc>) -> bool {
    baked_market_hours(symbol)
        .map(|m| m.is_blocked_at(now))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, TimeZone, Weekday};

    /// A UTC instant `day_offset` days after a known Monday (2026-07-06).
    fn at(day_offset: i64, hour: u32, minute: u32) -> DateTime<Utc> {
        let mon = Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap();
        mon + chrono::Duration::days(day_offset)
            + chrono::Duration::hours(hour as i64)
            + chrono::Duration::minutes(minute as i64)
    }

    #[test]
    fn weekend_mask_blocks_friday_night_through_sunday() {
        let m = weekend_mask();
        // Friday (day 4) 21:00 UTC — blocked (start of weekend halt).
        assert_eq!(at(4, 21, 0).weekday(), Weekday::Fri);
        assert!(m.is_blocked_at(at(4, 21, 0)), "Fri 21:00 blocked");
        assert!(m.is_blocked_at(at(5, 12, 0)), "Saturday blocked");
        assert!(m.is_blocked_at(at(6, 12, 0)), "Sunday midday blocked");
        assert!(
            m.is_blocked_at(at(6, 21, 59)),
            "Sun 21:59 blocked (in halt)"
        );
        // Reopens Sunday 22:00 UTC.
        assert!(!m.is_blocked_at(at(6, 22, 0)), "Sun 22:00 reopened");
        // Friday before the halt is allowed.
        assert!(!m.is_blocked_at(at(4, 20, 59)), "Fri 20:59 allowed");
    }

    #[test]
    fn weekend_mask_does_not_block_midweek() {
        // THE BUG CASE: a Tuesday 17:00 UTC bar (the wrongly-rejected EUR/USD
        // bar) must NOT be blocked by the weekend rule.
        let m = weekend_mask();
        assert_eq!(at(1, 17, 0).weekday(), Weekday::Tue);
        assert!(
            !m.is_blocked_at(at(1, 17, 0)),
            "Tuesday 17:00 must be allowed"
        );
        assert!(!m.is_blocked_at(at(2, 12, 0)), "Wednesday allowed");
        assert!(!m.is_blocked_at(at(3, 9, 0)), "Thursday allowed");
    }

    #[test]
    fn uncatalogued_symbol_falls_open() {
        assert!(baked_market_hours("NO_SUCH_SYMBOL_XYZ").is_none());
        assert!(!market_hours_blocked("NO_SUCH_SYMBOL_XYZ", at(4, 22, 0)));
    }
}
