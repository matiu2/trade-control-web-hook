# Market-hours blackout rebuild — candle-derived, weekday-aware

Replaces the broker-session-string deriver (which produced phantom daily FX
blackouts — see the `market-hours-blackout-weekly-gap-bug` memory) with a baked,
candle-measured, per-(venue,instrument) table. Weekend rule always-on; daily-close
rule only for instruments whose daily closes gap >1 ATR ≥20% of the time (≥30
samples). Mirrors the spread-hour baked table (`core/src/spread_blackout.rs`
`baked_candle_row` + generated `baseline_candle.rs`).

## Decisions (locked 2026-07-20)
- Window model: full-week bitmap `[bool; 7*1440]` (weekday+minute), replacing the
  day-blind minute-of-day `NoEntryWindow` for market-hours. Gate indexes by
  `(weekday, minute)` on `now: DateTime<Utc>`.
- Weekend rule: always-on, ALL instruments (Fri-close → Mon-open span).
- Daily-close rule: 20% threshold, ATR24, jump≥1×ATR, ≥30 samples — **counting
  only MID-WEEK (Mon–Thu) attention gaps**, because a Friday attention-gap IS the
  weekend gap and is already covered by the universal weekend rule. Counting
  midweek-only is what separates "this cash index has a real overnight daily
  close" from "this FX pair just gaps over the weekend". Derived from the scan
  JSON `events[].weekday` — reproducible, not hand-picked.
  - Resulting DAILY-CLOSE instruments (midweek frac ≥20%, ≥30 samples):
    - TN(7): SOUTHAFRICA40 (15h), AZNLSE (15/16h), ES35 (15/16h), EU50 (19/20h),
      DE40UK100 (19/20h), US500US2000ROLLINGFUTUREDIFF (19/20h), CH20 (19/20h).
    - OANDA(6): USDTRY (14/15h), TRYJPY (14/15h), EURTRY (14/15h), UK10YB (16/17h),
      NL25EUR (19/20h), FR40 (19/20h).
  - Daily block placed as `[peak_close_hour .. peak_close_hour+2h]` UTC (the two
    dominant close hours seen), applied every day (weekend days harmless — already
    weekend-blocked). No pre-close buffer for now; the reopen gap is what we're
    protecting the resting order from, and the block spans the close hour itself.

## Build order (ordered commits, keep each < ~600 lines)

- [x] **A. Weekly bitmap model + weekday-aware gate (core).** (commit 77aa437)
  - `WeekMask` (`[bool; 7*1440]`) in `core/src/intent/blackout/week_mask.rs`.
  - `is_blocked_at(now)`, `block_span`, `block_daily`. 7 unit tests.
- [x] **B. Generator + baked table.**
  - New `market-hours-gen` crate (workspace member), mirroring
    `spread-baseline-gen`: `lib/universe/fetch/compute/render` + `bin/generate`.
  - Self-fetches H1 candles (OANDA + TN), measures ATR-gaps, splits weekend vs
    **mid-week** attention (the refinement — Friday gaps ARE the weekend rule),
    emits `core/src/market_hours_baked.rs` (245 rows, 13 daily-close instruments).
  - `core::intent::blackout::baked` gets `baked_market_hours(symbol) -> Option<WeekMask>`
    (weekend always + daily overlay) and `market_hours_blocked(symbol, now)`.
- [x] **C. Rewire consumers, retire deriver.**
  - `run_enter` gate (`enter.rs`): `market_hours_blocked(&resolved.instrument, now)`
    replaces the `get_blackout_windows` + `is_inside_any` KV path.
  - Sweep/replay (`sweep_gate::market_blackout_due_symbol`, `simulator::sweep_reason`):
    keyed on `intent.instrument`; `blackout_windows` param dropped everywhere.
  - Replay driver: no more `market_info` fetch / KV seed (deleted
    `replay_candles/market_hours.rs`).
  - Cron: deleted `trade-control-cron/src/blackout_hours.rs` + retired the
    `blackout_hours_loop` from the worker scheduler (no daily refresh needed).
  - Left the `NoEntryWindow` + `windows_from_session` + KV `get/set_blackout_windows`
    trait methods in place as dead-for-now (harmless; a follow-up can delete).
- [x] **D. Tests + replay parity.**
  - Weekday gate unit tests in `baked.rs` (weekend Fri→Sun; NOT mid-week = the
    EUR/USD bug case; uncatalogued falls open).
  - Rewrote `enter_inside_market_hours_blackout_is_rejected_by_the_baked_gate`
    (Friday-night fire) and `sweep_reason_reports_market_blackout` (weekend vs
    mid-week) for the baked gate.
  - Generator: `compute`/`render`/`universe` unit tests (9).

## DONE — all steps complete. `market-hours-gen` regenerable:
`OANDA_TOKEN=... cargo run -p market-hours-gen --release -- --out ../core/src/market_hours_baked.rs`

## Notes
- Probes/data in scratchpad: `tn_gap_atr.json`, `oanda_gap_atr.json` (superseded
  by the self-fetching generator, kept as validation reference).
- Venue dimension matters: the midweek split put FR40-TN (18.9%) below and
  CH20-OANDA (19.2%) below the 20% line, while their cross-venue twins cleared —
  a TN-only table would have missed USDTRY (59% midweek) etc.
