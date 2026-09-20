//! The **trading session span** of a session-bound instrument, measured from
//! H1 candle presence — the second thing this generator bakes.
//!
//! The ATR-gap table says *which hours to block around a close*. It has no
//! notion of "this market is shut from 16:00 to 07:00" — and once D1/H4 bars
//! follow the instrument's session anchor, a bar that ends at the session close
//! is only *closed* (a full bucket later) while the market is shut. The entry
//! path needs to know that, so it can park the entry until the open instead of
//! sending an order into a closed market.
//!
//! Measured in the anchor's **local** wall clock (Spain 35 = 09:00–18:00
//! Europe/Madrid), not UTC, so the span is right in both DST seasons from one
//! row. Per venue: TradeNation serves Spain 35 for 9 hours, OANDA for 12.
//!
//! Deliberately conservative — a wrong row parks entries on an open market:
//!
//! - Only Tue–Thu bars count, so the weekend edges never look like a session.
//! - An hour is "open" when ≥ [`OPEN_FRACTION`] of sampled days have a bar in it.
//! - Fewer than [`MIN_CLOSED_HOURS`] closed hours ⇒ **no row**. A one- or
//!   two-hour daily break is the ATR-gap table's job (it blocks the close
//!   hours); this table is for a market that is genuinely shut overnight.
//! - The open hours must form ONE contiguous run (circularly). A market with a
//!   day session and a night session gets **no row** rather than a row that
//!   would call its night session "closed".

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Datelike, NaiveDate, Timelike, Weekday};
use instrument_lookup::Anchor;

use crate::compute::Bar;

/// Fraction of sampled days an hour must trade on to count as in-session.
pub const OPEN_FRACTION: f64 = 0.6;
/// A market closed for fewer hours than this per day gets no row.
pub const MIN_CLOSED_HOURS: usize = 4;
/// Fewer sampled days than this and no span is trusted.
pub const MIN_DAYS: usize = 20;

/// A contiguous local-time trading session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSpan {
    /// IANA timezone the hours are read in.
    pub timezone: String,
    /// First in-session local hour (0–23).
    pub start_hour: u8,
    /// In-session hours, 1–22.
    pub hours: u8,
}

/// Why an instrument got no span — printed in the report so a surprising
/// omission is visible rather than silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoSpan {
    TooFewDays(usize),
    RoundTheClock,
    SplitSession(Vec<u8>),
}

/// Measure `bars`' session in `anchor`'s local clock.
pub fn session_span(bars: &[Bar], anchor: Anchor) -> Result<SessionSpan, NoSpan> {
    let open = open_hours(bars, anchor)?;
    if open.len() + MIN_CLOSED_HOURS > 24 {
        return Err(NoSpan::RoundTheClock);
    }
    let start =
        run_start(&open).ok_or_else(|| NoSpan::SplitSession(open.iter().copied().collect()))?;
    Ok(SessionSpan {
        timezone: anchor.tz.name().to_string(),
        start_hour: start,
        hours: open.len() as u8,
    })
}

/// Local hours-of-day traded on ≥ [`OPEN_FRACTION`] of Tue–Thu days.
fn open_hours(bars: &[Bar], anchor: Anchor) -> Result<BTreeSet<u8>, NoSpan> {
    let mut days: BTreeSet<NaiveDate> = BTreeSet::new();
    let mut seen: BTreeMap<u8, BTreeSet<NaiveDate>> = BTreeMap::new();
    for bar in bars {
        let local = bar.t.with_timezone(&anchor.tz);
        if !matches!(local.weekday(), Weekday::Tue | Weekday::Wed | Weekday::Thu) {
            continue;
        }
        days.insert(local.date_naive());
        seen.entry(local.hour() as u8)
            .or_default()
            .insert(local.date_naive());
    }
    if days.len() < MIN_DAYS {
        return Err(NoSpan::TooFewDays(days.len()));
    }
    let needed = days.len() as f64 * OPEN_FRACTION;
    Ok(seen
        .into_iter()
        .filter(|(_, on)| on.len() as f64 >= needed)
        .map(|(h, _)| h)
        .collect())
}

/// The first hour of the single circular run `open` forms, or `None` when the
/// hours are empty or form more than one run.
fn run_start(open: &BTreeSet<u8>) -> Option<u8> {
    let starts: Vec<u8> = open
        .iter()
        .copied()
        .filter(|h| !open.contains(&((h + 23) % 24)))
        .collect();
    match starts.as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};

    const MADRID: Anchor = Anchor {
        tz: chrono_tz::Europe::Madrid,
        hour: 9,
    };

    /// 60 days of H1 bars at the given UTC hours (flat prices — only times matter).
    fn bars_at_utc_hours(from: &str, hours: &[u32]) -> Vec<Bar> {
        let day0 = from.parse::<NaiveDate>().expect("date");
        (0..60)
            .flat_map(|d| {
                hours.iter().map(move |h| Bar {
                    t: Utc.from_utc_datetime(
                        &(day0 + Duration::days(d))
                            .and_hms_opt(*h, 0, 0)
                            .expect("hms"),
                    ),
                    o: 1.0,
                    h: 1.0,
                    l: 1.0,
                    c: 1.0,
                })
            })
            .collect()
    }

    #[test]
    fn spain_35_on_tradenation_is_nine_to_six_madrid() {
        // Real TN hours, summer: H1 bars 07:00..=15:00Z.
        let bars = bars_at_utc_hours("2026-06-01", &(7..16).collect::<Vec<_>>());
        assert_eq!(
            session_span(&bars, MADRID),
            Ok(SessionSpan {
                timezone: "Europe/Madrid".into(),
                start_hour: 9,
                hours: 9
            })
        );
    }

    /// The point of measuring in LOCAL time: winter bars are an hour later in
    /// UTC and must give the same row.
    #[test]
    fn the_span_is_the_same_in_winter() {
        let bars = bars_at_utc_hours("2026-01-05", &(8..17).collect::<Vec<_>>());
        assert_eq!(
            session_span(&bars, MADRID).map(|s| (s.start_hour, s.hours)),
            Ok((9, 9))
        );
    }

    #[test]
    fn a_session_may_start_before_the_anchor() {
        // OANDA serves Spain 35 from 08:00 Madrid (06:00Z) for 12 hours.
        let bars = bars_at_utc_hours("2026-06-01", &(6..18).collect::<Vec<_>>());
        assert_eq!(
            session_span(&bars, MADRID).map(|s| (s.start_hour, s.hours)),
            Ok((8, 12))
        );
    }

    #[test]
    fn a_round_the_clock_market_gets_no_row() {
        let bars = bars_at_utc_hours("2026-06-01", &(0..24).collect::<Vec<_>>());
        assert_eq!(session_span(&bars, Anchor::FX), Err(NoSpan::RoundTheClock));
        // …nor does one with a short daily break (3 closed hours)…
        let bars = bars_at_utc_hours("2026-06-01", &(0..21).collect::<Vec<_>>());
        assert_eq!(session_span(&bars, Anchor::FX), Err(NoSpan::RoundTheClock));
        // …but four closed hours is a real overnight closure.
        let bars = bars_at_utc_hours("2026-06-01", &(0..20).collect::<Vec<_>>());
        assert_eq!(session_span(&bars, Anchor::FX).map(|s| s.hours), Ok(20));
    }

    #[test]
    fn a_day_and_a_night_session_get_no_row() {
        let hours: Vec<u32> = (0..6).chain(10..20).collect();
        let bars = bars_at_utc_hours("2026-06-01", &hours);
        assert!(matches!(
            session_span(&bars, Anchor::FX),
            Err(NoSpan::SplitSession(_))
        ));
    }

    #[test]
    fn a_session_crossing_local_midnight_is_one_run() {
        // 20:00..=03:00 UTC, read in UTC-ish (London summer = +1): wraps midnight.
        let hours: Vec<u32> = (20..24).chain(0..4).collect();
        let bars = bars_at_utc_hours("2026-06-01", &hours);
        let london = Anchor {
            tz: chrono_tz::Europe::London,
            hour: 21,
        };
        assert_eq!(
            session_span(&bars, london).map(|s| (s.start_hour, s.hours)),
            Ok((21, 8))
        );
    }

    #[test]
    fn a_sparse_hour_is_not_in_session() {
        // Hour 16Z trades on only 1 day in 3: below OPEN_FRACTION.
        let mut bars = bars_at_utc_hours("2026-06-01", &(7..16).collect::<Vec<_>>());
        bars.extend(
            bars_at_utc_hours("2026-06-01", &[16])
                .into_iter()
                .step_by(3),
        );
        assert_eq!(session_span(&bars, MADRID).map(|s| s.hours), Ok(9));
    }

    #[test]
    fn too_little_history_is_not_trusted() {
        let bars: Vec<Bar> = bars_at_utc_hours("2026-06-01", &[7, 8])
            .into_iter()
            .take(20)
            .collect();
        assert!(matches!(
            session_span(&bars, MADRID),
            Err(NoSpan::TooFewDays(_))
        ));
    }
}
