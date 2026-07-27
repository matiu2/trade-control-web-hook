//! Parsing the operator's `--start` / `--end` datetimes, shared by
//! `replay-candles` and `tv-arm`.
//!
//! ## Why this is shared rather than duplicated
//!
//! `tv-arm ... replay --start X` **passes `--start` through** to
//! `replay-candles`. Both then parse the same string. They used to do it with
//! different rules: `replay-candles` accepted Brisbane-local bare datetimes at
//! minute precision, while `tv-arm` called `DateTime::parse_from_rfc3339`
//! directly, which
//!
//! - **requires seconds** — `2026-06-19T17:00+10:00` is rejected
//!   ("input contains invalid characters"), only `17:00:00+10:00` parses;
//! - **rejects a bare local time** outright — `2026-06-19T17:00` is
//!   "premature end of input".
//!
//! So a `--start` the operator had been using with `replay-candles` failed when
//! moved to `tv-arm`, with an error that reads like the *timestamp* is
//! malformed rather than like the two tools disagree. Same flag, same string,
//! two answers.
//!
//! One parser removes the class of bug rather than the instance.
//!
//! ## The rules
//!
//! - An **explicit** offset or `Z` is honoured exactly as written.
//! - A **bare** datetime (no offset) is **Brisbane** (UTC+10, no DST) — the
//!   operator's zone, and the zone the replay report renders every candle,
//!   fill, and exit in, so a window flag reads the same way as the output it
//!   scopes.
//! - Both minute and second precision are accepted on every form.
//!
//! Brisbane is a *fixed* offset with no DST, which is why
//! `from_local_datetime(..).single()` can't hit the ambiguous-hour case in
//! practice — but it is still checked rather than assumed, because a future
//! change to a DST-observing zone would otherwise silently pick one side of the
//! fold.

use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use color_eyre::eyre::{Result, eyre};

/// Brisbane's fixed UTC offset in seconds (+10:00, no DST).
pub const BRISBANE_OFFSET_SECS: i32 = 10 * 3600;

/// Parse a `--start` / `--end` datetime. See the module doc for the rules.
///
/// The error is deliberately explicit about the accepted forms: this is an
/// operator typing a timestamp on a command line, and "not a valid datetime" on
/// its own gives them nothing to correct.
pub fn parse_start_end(s: &str) -> Result<DateTime<Utc>> {
    // Explicit offset / Z wins — honour exactly what was written.
    // `parse_from_rfc3339` requires seconds + a `Z`/offset; the `%z` forms below
    // also accept an offset with minute-only precision (`...T17:00+10:00`),
    // which RFC3339 rejects.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    // Normalise a trailing `Z` to `+0000` so the `%z` parser accepts minute- and
    // second-precision UTC (`...T07:00Z`), which RFC3339 rejects without seconds.
    let normalised = s.strip_suffix('Z').map(|body| format!("{body}+0000"));
    let candidate = normalised.as_deref().unwrap_or(s);
    for fmt in ["%Y-%m-%dT%H:%M:%S%z", "%Y-%m-%dT%H:%M%z"] {
        if let Ok(dt) = DateTime::parse_from_str(candidate, fmt) {
            return Ok(dt.with_timezone(&Utc));
        }
    }
    // Bare datetime (no offset) → interpret in Brisbane (+10), convert to UTC.
    let brisbane = FixedOffset::east_opt(BRISBANE_OFFSET_SECS)
        .ok_or_else(|| eyre!("10h is a valid fixed offset"))?;
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return brisbane
                .from_local_datetime(&naive)
                .single()
                .map(|dt| dt.with_timezone(&Utc))
                .ok_or_else(|| eyre!("{s:?} is ambiguous in Brisbane time"));
        }
    }
    Err(eyre!(
        "{s:?} is not a valid datetime (expected Brisbane YYYY-MM-DDTHH:MM[:SS], \
         or an explicit offset like ...T07:00Z / ...T17:00+10:00)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_datetime_is_brisbane_minute_and_second_precision() {
        // A bare (offset-less) datetime is Brisbane (+10). 17:00 Brisbane is
        // 07:00 UTC. Minute and second precision agree.
        let a = parse_start_end("2026-06-30T17:00").expect("minute precision");
        let b = parse_start_end("2026-06-30T17:00:00").expect("second precision");
        assert_eq!(a, b);
        assert_eq!(a, Utc.with_ymd_and_hms(2026, 6, 30, 7, 0, 0).unwrap());
    }

    #[test]
    fn explicit_offset_is_honoured() {
        // `+10:00` spelled out == the bare Brisbane reading.
        assert_eq!(
            parse_start_end("2026-06-30T17:00+10:00").expect("offset, minute precision"),
            parse_start_end("2026-06-30T17:00").expect("bare"),
        );
        // `Z` is UTC, not Brisbane.
        assert_eq!(
            parse_start_end("2026-06-30T07:00Z").expect("Z, minute precision"),
            Utc.with_ymd_and_hms(2026, 6, 30, 7, 0, 0).unwrap(),
        );
        // An arbitrary offset is respected: 09:00+02:00 == 07:00 UTC.
        assert_eq!(
            parse_start_end("2026-06-30T09:00:00+02:00").expect("arbitrary offset"),
            Utc.with_ymd_and_hms(2026, 6, 30, 7, 0, 0).unwrap(),
        );
    }

    /// The four forms `DateTime::parse_from_rfc3339` **rejects** — which is what
    /// `tv-arm --start` used to call, so each of these was a hard error there
    /// while working fine in `replay-candles`.
    ///
    /// This is the regression test for the split, so it asserts the *contrast*
    /// directly: raw RFC3339 fails, the shared parser succeeds.
    #[test]
    fn accepts_the_forms_raw_rfc3339_rejects() {
        for s in [
            "2026-06-19T17:00+10:00", // offset, no seconds
            "2026-06-19T17:00Z",      // Z, no seconds
            "2026-06-19T17:00",       // bare local, no seconds
            "2026-06-19T17:00:00",    // bare local, with seconds
        ] {
            assert!(
                DateTime::parse_from_rfc3339(s).is_err(),
                "{s:?} unexpectedly parses as raw RFC3339 — this test's premise is stale"
            );
            assert!(
                parse_start_end(s).is_ok(),
                "{s:?} must parse: it is exactly what an operator types"
            );
        }
    }

    /// The two tools must agree on every accepted form. `tv-arm ... replay`
    /// forwards `--start` verbatim, so a disagreement means the arm cursor and
    /// the replay window silently point at different instants.
    #[test]
    fn every_accepted_form_maps_to_the_same_instant() {
        let want = Utc.with_ymd_and_hms(2026, 6, 30, 7, 0, 0).unwrap();
        for s in [
            "2026-06-30T17:00",
            "2026-06-30T17:00:00",
            "2026-06-30T17:00+10:00",
            "2026-06-30T17:00:00+10:00",
            "2026-06-30T07:00Z",
            "2026-06-30T07:00:00Z",
            "2026-06-30T09:00+02:00",
        ] {
            assert_eq!(parse_start_end(s).expect(s), want, "disagreement on {s:?}");
        }
    }

    #[test]
    fn rejects_garbage_datetime() {
        assert!(parse_start_end("yesterday").is_err());
        assert!(parse_start_end("").is_err());
        // A date with no time is NOT accepted — the window flags are
        // instant-precise, and defaulting to midnight in some zone would be a
        // guess the operator never made.
        assert!(parse_start_end("2026-06-30").is_err());
    }

    /// The error names the accepted forms. An operator who mistypes a timestamp
    /// needs to know what to type instead.
    #[test]
    fn the_error_says_what_is_accepted() {
        let msg = parse_start_end("nope")
            .expect_err("must reject")
            .to_string();
        assert!(msg.contains("YYYY-MM-DDTHH:MM"), "msg = {msg}");
        assert!(
            msg.contains("+10:00") || msg.contains("Brisbane"),
            "msg = {msg}"
        );
    }
}
