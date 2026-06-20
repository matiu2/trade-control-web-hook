//! Render timestamps in Brisbane time (UTC+10) for the replay report.
//!
//! The broker (TradeNation) and the operator both work in Brisbane time, so the
//! replay report shows candle/fill/exit times as UTC+10 — matching what the
//! operator sees on their TradingView chart and in the broker UI, rather than
//! UTC. Brisbane has no daylight saving, so the offset is a fixed `+10:00`
//! year-round; we render it explicitly so a reader never has to guess the zone.

use chrono::{DateTime, FixedOffset, Utc};

/// Brisbane's fixed UTC offset in seconds (+10:00, no DST).
const BRISBANE_OFFSET_SECS: i32 = 10 * 3600;

/// Format a UTC instant as Brisbane time with an explicit `+10:00` suffix,
/// e.g. `2026-06-18 21:00:00 +10:00`. `%:z` renders the offset so the zone is
/// never ambiguous even though Brisbane is fixed.
pub fn bne(dt: DateTime<Utc>) -> String {
    let brisbane =
        FixedOffset::east_opt(BRISBANE_OFFSET_SECS).expect("10h is a valid fixed offset");
    dt.with_timezone(&brisbane)
        .format("%Y-%m-%d %H:%M:%S %:z")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn renders_utc_instant_as_brisbane() {
        let utc = Utc.with_ymd_and_hms(2026, 6, 18, 11, 0, 0).unwrap();
        // 11:00 UTC is 21:00 in Brisbane (+10).
        assert_eq!(bne(utc), "2026-06-18 21:00:00 +10:00");
    }

    #[test]
    fn rolls_the_date_forward_past_midnight() {
        // 20:00 UTC → 06:00 next day Brisbane.
        let utc = Utc.with_ymd_and_hms(2026, 6, 18, 20, 0, 0).unwrap();
        assert_eq!(bne(utc), "2026-06-19 06:00:00 +10:00");
    }
}
