# TODO — minute-based spread-hour mask (fix close-boundary bleed)

## Problem
The spread-baseline-gen mask generator samples each H1 bar's **close** spread.
When a spread-hour spike starts in the last few minutes of the *previous* hour,
that ramp contaminates the previous hour's p90 bucket, flagging a whole hour
that is actually calm for 53/60 minutes. Confirmed on OANDA GBP_AUD hour 20
(06:00 Bris): elevated only :53–:59 (the 21:00/07:00 spike bleeding back), yet
flagged as a full spread hour → mask `[20,21]` instead of the correct `[21]`.

## Fix
Derive each hour's **flag** from minute-level spread requiring the *bulk* of the
hour elevated (median minute-ratio ≥ 3× median), not the close. Keep the
**widen amount** as the p90 (spike-sized). A short end-of-hour ramp then can't
flag the hour.

## Steps
- [x] 1. TN adapter: page `get_bidask_candles` backward in ≤1000-bar chunks so
      M1 over a multi-day range returns full history (today it hard-caps at
      1000/req = ~1 day for M1). DONE: `chunk_windows` pure helper + paging loop;
      3 unit tests + live probe (2d→2851, 7d→7131 bars); 28 tests pass, clippy
      clean. **broker-tradenation-adapter**
- [x] 2. compute.rs: minute-aware profile (`profile_from_minutes` + `MinuteBar`)
      buckets minute spreads by UTC hour, flags on **p75** minute-ratio
      (`FLAG_PERCENTILE`, bulk-of-hour), widen = p90; shared `apply_gates` with
      the H1 path. DONE. NOTE: median (p50) was too strict — dropped EUR_USD's
      short-but-real 21:00 spike; p75 is the separating line (keeps EUR_USD[21],
      drops GBP_AUD hour-20 bleed). 6 minute tests incl the calibration
      regression. 15 tests pass.
- [x] 3. fetch.rs / generate.rs: `fetch_oanda_minutes` (fwd-paged M1) +
      `minutes_from_bidask` (TN via paged adapter); `--days` window (default 90).
      DONE.
- [x] 4. Validated on real data (30d minutes): OANDA GBP_AUD → `[21]` (was
      `[20,21]`), EUR_USD → `[21]`, XAU_USD → `[]`. All anchors PASS.
      TODO: still need a TN GBP/AUD re-bake + TN anchor pass (EUR/USD, AUD/CHF,
      Spot Gold) — run TN separately (slow, paged M1).
- [ ] 5. Update committed `core/src/spread_baseline_candle.rs` (mask table).
      Regression test in core: OANDA GBP_AUD is `[21]` not `[20,21]`.
- [ ] 6. clippy + fmt; commit; (staging deploy is the user's call — it restarts
      the live demo worker).

## DST decision (user, 2026-07-14)
- **Keep UTC-hour masks**; do NOT convert to NY-local. The NY-close spike is
  21:00 UTC in US-summer / 22:00 UTC in US-winter, so a mask is only valid for
  its DST season and MUST be re-baked at each DST transition (operational
  contract, ~2×/year). See `[[spread_hour_dst_no_theoretical_model]]`.
- **Fetch window must stay WITHIN one DST season.** A literal 12-month window
  bucketed in UTC would smear the spike across BOTH 21:00 and 22:00 (each ~half
  the days) → the median flag test sees each hour elevated only ~50% of days →
  could flag NEITHER. So the generator takes a `--days` window (default sized to
  the current season, e.g. ≤ ~120d) and the operator re-bakes per season. Pull
  "a lot" (robust hot-day coverage) but not across a DST boundary.

## Notes / decisions
- Flag statistic = MEDIAN minute-ratio (hot-day hour-20 median 1.78× < 3×;
  p90 = 3.07× would still mis-flag). Widen = p90 (spike magnitude).
- OANDA hour 20 genuinely spikes ~18/83 days BUT only in the last ~6 min → not
  a real 6am spread hour; correctly dropped by the median test.
- Do NOT hand-edit the mask bit — fix the generator so every instrument is
  immunised against the bleed.
- Related (separate, in progress on staging): System-2 widen sub-hour lead /
  full-bar pre-arm. See TODO.md / BUG-spread-hour-widen-no-subhour-lead.md.
