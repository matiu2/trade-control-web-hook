# SCOPING — DST-aware spread-hour masks (market-local time + chrono-tz)

## Goal
Make the spread-hour mask correct **year-round** with no seasonal re-bake, by
anchoring each instrument's spread hour to its **governing market's local
wall-clock** and converting local→UTC at gate-check via `chrono-tz`. Fixes the
latent bug that today's UTC-hour masks (baked in US-summer) are an hour wrong
Nov–Mar.

## Evidence (this session)
- Spread hour = **5pm New York** for ALL FX + gold + US indices; confirmed on
  OANDA GBP_AUD H1: 22:00 UTC (EST) → 21:00 UTC (EDT) exactly at 2026-03-08.
  [[spread_hour_tracks_us_dst_confirmed]]
- Full per-class market mapping (which tz governs each):
  [[spread_hour_dst_per_market_mapping]].
- The committed table's FX rows are ALL `[20,21]` — the same hour-20
  close-boundary bleed the minute-based generator (already committed here) fixes
  → the re-bake drops hour 20 table-wide, independent of DST.

## Design

### Where the spread hour is stored
Change the mask's time basis from **UTC hour** to **governing-market-local
hour**. Two representations to choose (DECISION 1 below):
- **(a) Local-hour mask + tz-id per row.** Table row gains a tz-id string
  (`"America/New_York"`, `"Europe/London"`, `"Australia/Sydney"`,
  `"Asia/Hong_Kong"`, …). Mask bits are LOCAL hours. Gate converts `now`→that
  tz, indexes by local hour.
- **(b) Keep UTC mask, store the source tz + bake per-season is avoided by
  recomputing the UTC offset at check.** Rejected — you can't shift a baked UTC
  mask at runtime without knowing the local anchor; (a) is cleaner.

→ Go with **(a)**.

### Which timezone governs an instrument — RELATIONAL (settled)
Normalized, not a tz-per-row. In `instrument-lookup`:
- **A `[[spread_schedule]]` section** — named regimes, each `{ name, timezone }`:
  `ny`=America/New_York, `london`=Europe/London, `frankfurt`=Europe/Berlin,
  `zurich`=Europe/Zurich, `sydney`=Australia/Sydney, `hongkong`=Asia/Hong_Kong,
  `singapore`=Asia/Singapore, `tokyo`=Asia/Tokyo, `none`=no spread hour.
- **Each `Asset` carries `spread_schedule: String`** — a FK naming one regime.
  **Explicit on every asset** (one-time migration stamps all 1321 rows by class:
  Forex/Gold/Crypto/Commodity/Bond→`ny`; Index→its exchange regime; Stock→
  `none` for now). After the migration it's a real per-row FK, no runtime
  class-defaulting.
- Resolver `asset.spread_schedule_tz()` → the regime's `timezone` (chrono-tz
  zone); `none`→ no spread hour (mask stays empty, gate falls back).
- Static test: every asset's FK resolves to a defined schedule (no dangling
  refs). Generator cross-check (below) confirms the tz is actually CORRECT.

### Gate change (`core/src/spread_blackout.rs`)
- `is_spread_hour(instrument, now_utc)`: look up the instrument's tz, convert
  `now_utc` → local, index the LOCAL-hour mask. Same for
  `mask_active_with_lead` and `spread_hour_widen_for` (the lead arithmetic moves
  to local minutes — trivially the same since offsets are whole hours except
  a few 30/45-min zones, none in our set).
- chrono-tz `LocalResult::Ambiguous/None` at DST transition hours: 5pm is
  outside the 01–03 transition window, so safe; still handle explicitly
  (`.earliest()`) rather than `.unwrap()`.
- Fallbacks unchanged: absent/`reviewed=false` → NY-close-edge default.

### Generator change (`spread-baseline-gen`)
- Bucket minute spreads by **local hour in the instrument's governing tz**
  (convert each candle's UTC ts → that tz). Everything else (p75 flag, p90
  widen, med3+peak-frac) identical.
- Emit the tz-id into each table row.
- A full year of data now SHARPENS (all of it lands in the same local hour)
  instead of smearing across two UTC hours → can widen `--days` back up.

### Table format (`core/src/spread_baseline_candle.rs`)
Row becomes `(broker, symbol, tz_id, reviewed, mask_local, widen[24])`.
`mask_local` bits are local hours. Migration: regenerate the whole table.

## Migration / sequencing
1. instrument-lookup: add `spread_hour_tz` field + overlay + defaults per class.
   (Its own repo — commit + tag + bump path-dep.)
2. core gate: tz-aware `is_spread_hour` / widen; new table tuple; unit tests
   with a synthetic DST-crossing clock (a summer + winter `now` both map to the
   same local mask bit).
3. generator: local-hour bucketing + tz-id emission; re-bake full table.
4. Swap committed table; core regression tests (GBP_AUD summer AND winter `now`
   both spread-hour; an Asian index fixed year-round).
5. Replayer parity: the replay calls the SAME core gate, so it inherits the fix
   — add a replay test crossing a DST boundary.
6. clippy/fmt; deploy is user's call.

## Mechanism (why 5pm NY, and does London matter)
The spike is the **5pm-ET rollover instant**, driven by TWO compounding causes at
the same wall-clock: (a) the T+2 value-date / swap rollover bookkeeping, and (b)
a liquidity vacuum in the **seam between NY rollover and the Asia-Pacific open** —
NY is rolling over, London closed hours earlier (~15:00-16:00 UTC), Sydney/Tokyo
not yet ramped, only Wellington barely open. So it's NOT "after NY close"; NY is
still the reference and it's the rollover moment.
- **London does NOT drive the FX/gold spread hour** (its session boundary is
  hours before the spike) → **Europe/London DST is irrelevant to FX/gold masks.**
- London DOES govern **UK 100 (FTSE)**; Europe/Berlin governs DAX/SMI/EuroStoxx.
  So London/EU DST matters for those INDEX rows only.

## Validation still to run (user request)
Download exchange **open/close hours** and overlay on the measured spread
windows, to empirically confirm the per-market anchor:
- an FX pair's spike aligns to **5pm NY** and ignores London → US DST only.
- an Asian index (HK50) aligns to **HKEX session + lunch**, fixed in UTC,
  ignores NY → no DST. This is the clean discriminator.

## DECISIONS (settled 2026-07-15)
1. **Table representation (a)** — local-hour mask + tz-id per row.
2. **TZ source: add `spread_hour_tz` to instrument-lookup.** FX/gold/US-index
   default `America/New_York`; European indices `Europe/London` (FTSE) /
   `Europe/Berlin` (DAX/EuroStoxx) / `Europe/Zurich` (SMI); ASX
   `Australia/Sydney`; Asian indices fixed-UTC zones (`Asia/Hong_Kong`,
   `Asia/Singapore`, `Asia/Tokyo`). Overlay-able.
3. **All classes at once**, and **regenerate the whole table** minute-based with
   local-hour bucketing (also lands the hour-20 bleed fix table-wide).

## EMPIRICAL VALIDATION (done) — spike = NY 5pm, London irrelevant
tz-computed session boundaries vs measured GBP/AUD spike:
- SUMMER: NY 5pm rollover = 21:00 UTC = measured spike onset (21:04). London
  close = 15:30 UTC (5½h earlier — NOT the driver). Sydney/Wellington open =
  21:00 UTC (the seam).
- WINTER: NY 5pm = 22:00 UTC = measured winter spike. London close 16:30 UTC.
So FX/gold anchor to America/New_York; London/EU DST governs only European
INDEX rows. Asian-index spread hour is ~absent on OANDA (indices tight
around the clock) — fixed-UTC zones are correct-by-default, nothing to bake.

## Non-goals / preserve
- Don't hardcode DST dates — chrono-tz only.
- Don't break the `reviewed` verdict, med3+peak-frac, ON/OFF asymmetry, or the
  NY-close-edge fallback.
- Any gate change lands in BOTH replayer and worker (they share the core seam).
