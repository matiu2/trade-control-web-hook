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
- Daily-close rule: 20% threshold, ATR24, jump≥1×ATR, ≥30 samples.
  - TN(9): SouthAfrica40, AZNLSE, ES35, EU50, DE40UK100, US500US2000diff, CH20,
    FR40, US30USTEC.
  - OANDA(8): USDTRY, TRYJPY, EURTRY, UK10YB, NL25EUR, FR40, CH20, ESPIXEUR.

## Build order (ordered commits, keep each < ~600 lines)

- [ ] **A. Weekly bitmap model + weekday-aware gate (core).**
  - New `WeekMask` type (`[bool; 7*1440]` or bitset) in `core/src/intent/blackout/`.
  - `is_blocked_at(now: DateTime<Utc>) -> bool` indexing `(weekday*1440 + minute)`.
  - Constructors: `block_span(from_weekday, from_min, to_weekday, to_min)` for the
    weekend; `block_daily(minute_range)` (all 7 days) for daily closes.
  - Keep the old `NoEntryWindow` + `is_inside_any` for now (temp fix holds); the
    new gate is additive until C rewires.
- [ ] **B. Generator + baked table.**
  - Promote `cli/examples/{tn,oanda}_gap_atr.rs` into a `market-hours-gen` tool
    that emits a Rust `MARKET_HOURS[(venue,symbol) -> WeekMask-spec]` table.
  - `core` gets `baked_market_hours(venue, instrument) -> Option<WeekMask>` scan,
    mirroring `baked_candle_row`.
- [ ] **C. Rewire consumers, retire deriver.**
  - `run_enter` gate (`core/src/dispatch/enter.rs`): consult baked table + weekend
    rule via `is_blocked_at(now)` instead of `get_blackout_windows` +
    `is_inside_any(now_utc_minute_of_day)`.
  - Sweep (`core/src/sweep_gate.rs::market_blackout_due`): same.
  - Cron (`trade-control-cron/src/blackout_hours.rs`) + replay
    (`cli/src/bin/replay_candles/market_hours.rs`): stop calling
    `windows_from_session`/`market_info`; the baked table needs no daily refresh.
  - Delete `windows_from_session` + the `NoEntryWindow` KV get/set path once
    nothing reads it (or leave the KV trait methods as dead for a follow-up).
- [ ] **D. Tests + replay parity.**
  - Weekday gate unit tests (weekend wrap Fri→Mon; daily block on the right hour;
    NOT blocked mid-week for a weekend-only instrument = the EUR/USD bug case).
  - Extend `enter_inside_market_hours_blackout_is_rejected` for weekday cases.
  - Generator output test.

## Notes
- Probes/data in scratchpad: `tn_gap_atr.json`, `oanda_gap_atr.json`.
- Venue dimension matters: only CH20+FR40 overlap; a TN-only table misses
  USDTRY (80% daily gap) etc.
