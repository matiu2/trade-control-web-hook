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

/// True when the NY-close edge hour falls anywhere inside the half-open span
/// `(prev, now]` — the **span-aware** twin of [`is_ny_close_edge`].
///
/// # Why a span predicate exists
///
/// The live worker samples the edge on a wall-clock loop (`upkeep_secs`,
/// default 900 s), so it hits the close hour ~4× and reliably opens the
/// spread-blackout window marker. The offline replay only has one instant per
/// cron tick — the newest bar's close — so it evaluates [`is_ny_close_edge`]
/// **once per tick**. Whenever a tick's span is longer than an hour, or lands
/// off the close hour, that single sample can step straight over the edge and
/// the marker is never opened: `run_enter`'s System-1 spread-blackout gate
/// then fails OPEN offline while live rejects.
///
/// This predicate asks the live question ("was the close hour anywhere in the
/// stretch of time this tick covers?") instead of the instant question, so a
/// replay tick that spans the edge behaves like live's four samples inside it.
///
/// `prev` is the *previous* tick instant (exclusive) and `now` the current one
/// (inclusive), so consecutive ticks partition time with no double-count and no
/// gap. A `prev >= now` span is empty and answers `false`. The scan walks whole
/// hours, so the season is re-evaluated per hour and a span crossing the
/// EDT↔EST switch is handled correctly.
pub fn ny_close_edge_in_span(prev: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    last_ny_close_edge_in_span(prev, now).is_some()
}

/// The **latest** NY-close-edge instant inside `(prev, now]`, or `None` when
/// the span contains none. [`ny_close_edge_in_span`] is the boolean face of it.
///
/// # Why the caller wants the instant, not just the boolean
///
/// The window marker this feeds carries a TTL measured *from the moment it was
/// opened*. Live opens it at a wall-clock instant inside the close hour, so the
/// window lapses ~3 h after the NY close. A replay tick that merely *spans* the
/// edge is stamped at the tick's own `now`, which can be hours later — stamping
/// there would push the lapse well past where live lets it go, trading a
/// fail-open divergence for a fail-closed one. Returning the edge instant lets
/// the caller stamp the marker where live would have.
///
/// **Latest**, not earliest, because live's last sample inside the close hour
/// is what sets the final TTL; a span covering several days must lapse from the
/// most recent close, not the first one it happened to cover.
pub fn last_ny_close_edge_in_span(
    prev: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if prev >= now {
        return None;
    }
    // `now` is in the span (half-open: exclusive at `prev`, inclusive at `now`)
    // and is its latest instant, so it wins outright when it is on the edge.
    // This is also what keeps a sub-hourly tick identical to the instant gate.
    if is_ny_close_edge(now) {
        return Some(now);
    }
    // Otherwise walk the top of each whole hour strictly inside `(prev, now)` —
    // only the *hour* matters — keeping the last match. Starting one hour past
    // the floor of `prev` excludes `prev`'s own hour, so consecutive ticks
    // partition time with no double-count and no gap.
    //
    // `with_minute(0)` / `with_second(0)` cannot fail for a real timestamp; the
    // `?` is a defensive bail rather than a loop on a bad value.
    let floored = prev.with_minute(0).and_then(|t| t.with_second(0))?;
    let mut probe = floored + chrono::Duration::hours(1);
    let mut last = None;
    while probe < now {
        // Re-derive the season at EACH probe hour: a span straddling the
        // EDT↔EST switch contains hours from both, and the close hour differs
        // between them (21:00 vs 22:00 UTC).
        if is_ny_close_edge(probe) {
            last = Some(probe);
        }
        probe += chrono::Duration::hours(1);
    }
    last
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

    // --- span predicate: the replay's once-per-tick sampling gap ---

    /// The motivating miss. A D1 series is midnight-UTC anchored, so the
    /// replay's per-tick `now` (bar open + 24 h) is always 00:00 UTC and
    /// NEVER equals the close hour — the instant predicate is false for
    /// every bar of a whole year, so the marker is never opened offline
    /// while live opens it daily.
    #[test]
    fn daily_bars_never_hit_the_instant_edge_but_always_hit_the_span() {
        let mut instant_hits = 0;
        let mut span_hits = 0;
        let mut prev = ts("2026-06-01T00:00:00Z");
        for day in 1..=30 {
            let now = ts(&format!("2026-06-{day:02}T00:00:00Z")) + chrono::Duration::days(1);
            if is_ny_close_edge(now) {
                instant_hits += 1;
            }
            if ny_close_edge_in_span(prev, now) {
                span_hits += 1;
            }
            prev = now;
        }
        assert_eq!(
            instant_hits, 0,
            "midnight-anchored D1 closes never land on the 21:00/22:00 close hour"
        );
        assert_eq!(
            span_hits, 30,
            "every D1 bar's span contains exactly one NY close hour"
        );
    }

    /// The corpus's H4 grid is NY-session anchored (21:00 UTC in EDT), so its
    /// bar closes land exactly ON the edge. The span predicate must agree with
    /// the instant one here — this is what keeps the H4 fixtures unchanged.
    #[test]
    fn session_anchored_h4_agrees_instant_and_span() {
        // 2026-07 is EDT: grid 21/01/05/09/13/17, edge hour 21.
        let mut prev = ts("2026-07-06T17:00:00Z");
        let mut instant_hits = 0;
        let mut span_hits = 0;
        for step in 1..=30 {
            let now = prev + chrono::Duration::hours(4);
            if is_ny_close_edge(now) {
                instant_hits += 1;
            }
            if ny_close_edge_in_span(prev, now) {
                span_hits += 1;
            }
            prev = now;
            let _ = step;
        }
        assert_eq!(
            instant_hits, span_hits,
            "an NY-session-anchored H4 series must score identically under both predicates"
        );
        assert!(instant_hits >= 5, "sanity: the window spans several days");
    }

    /// A sub-hourly tick must NOT fire on every tick inside the hour it lands
    /// in — it fires when the instant is in the close hour, exactly like live's
    /// repeated 15-min samples. This pins that the span predicate is a
    /// superset of the instant one, never a replacement that shifts it.
    #[test]
    fn sub_hourly_ticks_match_the_instant_predicate_exactly() {
        // 2026-07-06 is EDT, edge hour 21:00 UTC. Walk M15 across 20:00–23:00.
        let mut prev = ts("2026-07-06T20:00:00Z");
        for step in 1..=12 {
            let now = prev + chrono::Duration::minutes(15);
            assert_eq!(
                ny_close_edge_in_span(prev, now),
                is_ny_close_edge(now),
                "M15 step {step} ({now}) must not diverge from the instant gate"
            );
            prev = now;
        }
    }

    /// An H4 series whose grid is NOT session-anchored (a midnight-UTC H4,
    /// which is what a non-FX venue or a re-aligned cache would produce)
    /// steps over the close hour: 20:00 → 00:00 skips 21:00 entirely.
    #[test]
    fn midnight_anchored_h4_steps_over_the_edge() {
        let prev = ts("2026-07-06T20:00:00Z");
        let now = ts("2026-07-07T00:00:00Z");
        assert!(
            !is_ny_close_edge(now),
            "00:00 is not the close hour — the instant gate misses"
        );
        assert!(
            ny_close_edge_in_span(prev, now),
            "21:00 EDT lies inside (20:00, 00:00] — the span gate must catch it"
        );
    }

    /// A span crossing the EDT→EST switch must re-evaluate the season per hour,
    /// not once for the whole span. 2026-11-01 is the 1st Sunday of November,
    /// so the edge moves from 21:00 to 22:00 UTC that day.
    #[test]
    fn span_crossing_the_dst_switch_finds_the_right_hour() {
        // Saturday 31-Oct is EDT (edge 21:00); Sunday 1-Nov is EST (edge 22:00).
        let sat_prev = ts("2026-10-31T19:00:00Z");
        let sat_now = ts("2026-10-31T23:00:00Z");
        assert!(
            ny_close_edge_in_span(sat_prev, sat_now),
            "EDT day: 21:00 is inside the span"
        );
        // A Sunday span that contains 21:00 but NOT 22:00 must be false —
        // proving the season is read at the probe hour, not assumed EDT.
        let sun_prev = ts("2026-11-01T19:00:00Z");
        let sun_miss = ts("2026-11-01T21:30:00Z");
        assert!(
            !ny_close_edge_in_span(sun_prev, sun_miss),
            "EST day: 21:00 is NOT the close hour, so this span must miss"
        );
        let sun_hit = ts("2026-11-01T23:00:00Z");
        assert!(
            ny_close_edge_in_span(sun_prev, sun_hit),
            "EST day: 22:00 is inside the span"
        );
        // Span opens on the EDT Saturday, ends on the EST Sunday, and the ONLY
        // close hour inside it is the EDT 21:00 (it stops before Sat 22:00, and
        // Sun 22:00 is beyond it). An implementation that reads the season once
        // from `now` (EST ⇒ hunts 22:00) finds nothing and answers false.
        assert!(
            ny_close_edge_in_span(ts("2026-10-31T20:30:00Z"), ts("2026-10-31T21:30:00Z")),
            "the only close hour in this span is the EDT 21:00; a now-fixed season misses it"
        );
        // The mirror: the only close hour inside is the EST 22:00, and the span
        // opens back in EDT territory. An implementation reading the season once
        // from `prev` (EDT ⇒ hunts 21:00) finds only Sun 21:00, which is NOT a
        // close hour that day, and answers false.
        assert!(
            ny_close_edge_in_span(ts("2026-11-01T20:30:00Z"), ts("2026-11-01T22:30:00Z")),
            "the only close hour in this span is the EST 22:00"
        );
        // And a straddling span containing NEITHER close hour must be false —
        // otherwise the two asserts above pass on a predicate that just says
        // "true for any long span".
        assert!(
            !ny_close_edge_in_span(ts("2026-10-31T23:30:00Z"), ts("2026-11-01T20:30:00Z")),
            "straddling span between the two close hours contains neither"
        );
        // The sharpest straddle. `prev` sits exactly ON the EDT close hour
        // (excluded, half-open) and `now` is Sunday 00:00 (EST season, not an
        // edge). The only probe hours inside are Sat 22:00/23:00 EDT and Sun
        // 00:00 — none of which is a close hour in ITS OWN season. An
        // implementation that reads the season once from `now` (EST ⇒ hunts
        // 22:00) wrongly matches Saturday's 22:00 and answers true.
        assert!(
            !ny_close_edge_in_span(ts("2026-10-31T21:00:00Z"), ts("2026-11-01T00:00:00Z")),
            "Sat 22:00 UTC is EDT, so it is NOT a close hour — the season must be \
             re-read at each probe hour, never fixed from either end of the span"
        );
    }

    /// An empty or inverted span is false — no marker opens from a degenerate
    /// tick (the replay's first tick, where `prev == now`).
    #[test]
    fn empty_or_inverted_span_is_false() {
        let t = ts("2026-07-06T21:00:00Z");
        assert!(
            !ny_close_edge_in_span(t, t),
            "an empty span opens nothing, even sitting on the edge hour"
        );
        assert!(
            !ny_close_edge_in_span(ts("2026-07-06T22:00:00Z"), ts("2026-07-06T20:00:00Z")),
            "an inverted span is empty"
        );
    }

    /// A span far longer than a day (a big cron catch-up / a weekly bar) still
    /// answers true — and a long span nowhere near a close hour cannot exist,
    /// so the negative case is a short one well clear of the edge.
    #[test]
    fn long_span_hits_and_short_off_edge_span_misses() {
        assert!(
            ny_close_edge_in_span(ts("2026-07-06T00:00:00Z"), ts("2026-07-13T00:00:00Z")),
            "a week-long span contains many close hours"
        );
        assert!(
            !ny_close_edge_in_span(ts("2026-07-06T09:00:00Z"), ts("2026-07-06T13:00:00Z")),
            "09:00→13:00 EDT is nowhere near the 21:00 close hour"
        );
    }

    /// The marker's TTL runs from when it was OPENED, so a span that stepped
    /// over the edge must report the edge instant — not the tick's own `now`,
    /// which can be hours later and would hold the window open past where live
    /// lets it lapse.
    #[test]
    fn span_reports_the_edge_instant_not_the_tick_end() {
        // Midnight-anchored D1 bar closing 2026-07-07T00:00Z; the EDT close
        // hour it stepped over is 2026-07-06T21:00Z — 3 h earlier.
        let edge =
            last_ny_close_edge_in_span(ts("2026-07-06T00:00:00Z"), ts("2026-07-07T00:00:00Z"))
                .expect("the span contains an edge");
        assert_eq!(
            edge,
            ts("2026-07-06T21:00:00Z"),
            "must stamp at the close hour, not at the tick end"
        );
    }

    /// A tick landing ON the edge stamps at the tick instant itself (minutes
    /// into the hour included), exactly as live does — not rewound to the top
    /// of the hour, which would shorten the window by up to an hour.
    #[test]
    fn span_ending_on_the_edge_stamps_the_tick_instant() {
        let now = ts("2026-07-06T21:45:00Z");
        assert_eq!(
            last_ny_close_edge_in_span(ts("2026-07-06T17:00:00Z"), now),
            Some(now),
            "an on-edge tick stamps where it actually is"
        );
    }

    /// A multi-day span lapses from the MOST RECENT close, not the first one it
    /// covered — an earliest-match implementation would open a window that is
    /// already expired.
    #[test]
    fn multi_day_span_reports_the_latest_edge() {
        assert_eq!(
            last_ny_close_edge_in_span(ts("2026-07-06T00:00:00Z"), ts("2026-07-09T12:00:00Z")),
            Some(ts("2026-07-08T21:00:00Z")),
            "the latest close hour inside the span, not the earliest"
        );
    }

    /// Across **contiguous** bars the corpus's two granularities are unchanged
    /// by the span gate: an H1 tick spans exactly one hour, and an H4 tick lands
    /// on the NY-session grid (21:00 UTC in EDT, 22:00 in EST), so in both cases
    /// the span's only candidate hour IS the tick instant. Walk a fortnight of
    /// each across the DST switch and assert per-tick equality with the instant
    /// gate — the reason the great majority of fixture cells do not move.
    ///
    /// ⚠️ **Contiguity is the load-bearing premise, and real series break it.**
    /// An FX week has a ~50 h weekend gap, so the tick at the Sunday reopen
    /// spans two NY closes on ANY granularity — which is exactly the case this
    /// fix is for (live's wall-clock loop ran through those closes; replay's
    /// once-per-bar sample did not). Twelve `xau-xag-h1-2026-08-07` cells move
    /// for precisely that reason, and correctly: a 94-pip Sunday-reopen spread
    /// against a 36-pip threshold. So do NOT read this test as "H1 never moves"
    /// — it says "H1 never moves *where the bars are contiguous*".
    #[test]
    fn contiguous_h1_and_h4_ticks_are_identical_under_both_gates() {
        // H1: a fixed one-hour step, fourteen days across the switch.
        let mut prev = ts("2026-10-25T00:00:00Z");
        for tick in 0..(14 * 24) {
            let now = prev + chrono::Duration::hours(1);
            assert_eq!(
                ny_close_edge_in_span(prev, now),
                is_ny_close_edge(now),
                "H1 tick {tick} ({prev} -> {now}) must be identical under both \
                 gates, or the fixture corpus moves"
            );
            prev = now;
        }

        // H4 on the REAL NY-session grid. It is NOT a fixed 4 h step across the
        // DST switch: the grid re-anchors to NY 17:00, so the UTC boundaries
        // move 21/01/05/09/13/17 (EDT) -> 22/02/06/10/14/18 (EST), which makes
        // the transition bar five hours long. Generating it by stepping a fixed
        // 4 h from an EDT anchor drifts OFF the grid in EST and manufactures a
        // divergence the real corpus cannot have — so build the boundaries from
        // the anchor rule instead.
        let mut prev = ts("2026-10-25T21:00:00Z");
        for tick in 0..(14 * 6) {
            // The next session boundary: step 4 h, then snap onto the close-hour
            // grid for whatever season that instant is in.
            let naive = prev + chrono::Duration::hours(4);
            let close_hour = if ny_is_edt(naive.date_naive()) {
                NY_CLOSE_HOUR_UTC_EDT
            } else {
                NY_CLOSE_HOUR_UTC_EST
            };
            // Boundaries sit at close_hour + 4k (mod 24).
            let drift = (naive.hour() + 24 - close_hour) % 4;
            let now = naive + chrono::Duration::hours((4 - drift) as i64 % 4);
            assert_eq!(
                ny_close_edge_in_span(prev, now),
                is_ny_close_edge(now),
                "H4 session tick {tick} ({prev} -> {now}) must be identical under \
                 both gates, or the fixture corpus moves"
            );
            prev = now;
        }
    }

    /// The FX **weekend gap** — the case that actually moves the corpus, and the
    /// one no contiguous-bar reasoning reaches.
    ///
    /// An FX week closes Friday ~21:00 UTC and reopens Sunday 21:00/22:00 UTC,
    /// so the replay tick at the reopen bar spans ~50 h on ANY granularity. Live
    /// ran its wall-clock upkeep loop straight through the NY closes inside that
    /// gap and opened the marker; replay's once-per-bar sample never did, so the
    /// Sunday-reopen spread spike — the widest of the week — went ungated.
    ///
    /// Timestamps are the real `xau-xag-h1-2026-08-07` fixture's: the last bar
    /// before the gap opens Fri 2026-08-07T20:00Z (closes 21:00Z) and the reopen
    /// bar opens Sun 2026-08-09T22:00Z (closes 23:00Z).
    #[test]
    fn the_fx_weekend_gap_tick_spans_the_ny_close() {
        let fri_close = ts("2026-08-07T21:00:00Z");
        let sun_reopen_close = ts("2026-08-09T23:00:00Z");
        assert!(
            !is_ny_close_edge(sun_reopen_close),
            "the reopen bar's close is 23:00Z — not the 21:00Z EDT close hour, \
             which is why the instant gate missed it"
        );
        assert_eq!(
            last_ny_close_edge_in_span(fri_close, sun_reopen_close),
            Some(ts("2026-08-09T21:00:00Z")),
            "the weekend span covers Saturday's and Sunday's NY closes; the \
             marker must open at the LATEST one, so it lapses at 2026-08-10T00:00Z"
        );
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
