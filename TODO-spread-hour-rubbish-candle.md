# TODO — spread-hour "rubbish candle" suppression

Scoping: `SCOPING-spread-hour-rubbish-candle.md`. Predicate exists:
`core::spread_blackout::is_spread_hour(instrument, now)` (baked clock + 30-min
lead + NY-close fallback for un-sampled).

Suppress on spread-hour bars (shared engine, so replay == worker):
new entries, signal detection, level crosses. NOT stop-outs.

## Steps

- [x] 1. engine `evaluate.rs`: gate the **entry** path — leading guard in
      `evaluate_one_entry` (beside the `replay_start` floor). Covers PinePattern
      + M/W enters.
- [x] 2. engine `evaluate.rs`: gate **level crosses** in `fire_rule` — suppress
      the returned `hit` AFTER `record_last_close` runs (no OnClose desync).
      Covers break-and-close, guards, invalidation, control, M/W crosses.
- [x] 3. engine `evaluate.rs`: gate the **retest** in `stamp_retest` — wrap the
      stamp/fire condition, keep `record_last_close` at the tail.
- [x] 4. engine `evaluate.rs`: gate **reversal-close signal detection** — the
      `eval_pine_guard` arm in `evaluate_guards`.
- [x] 5. engine `simulator.rs`: gate the **replay fill scan** in `find_fill` —
      order stays resting; next clean bar fills.
- [x] 6. Detector-summary visibility (`verbose.rs`): a suppressed spread-hour
      bar with a mark shows `⌀ spread-hour (rubbish candle)`.
- [x] 7. Regression: predicate-false paths byte-identical (816 core + 144
      engine + all cli tests green).
- [x] 8. cargo clippy + fmt pass.
- [x] 9. Confirmed `is_spread_hour("AUD/CHF", 2026-07-08T21:00Z)` == true via
      the real baked mask (bit 21, 12p) — the live worker's `evaluate_plan`
      (trade-control-cron/engine.rs:182) now suppresses that bar's entry.
- [ ] 10. README + CHANGELOG; commit/push/tag; advance parent pointer.

## Design notes (final)
- The gate lives entirely in shared `engine` + pure `core` predicate. The live
  worker consumes the SAME `evaluate_plan`, so entry/signal/cross suppression is
  live automatically — no worker-side code. The `find_fill` gate is replay-only
  (worker uses real broker fills; its engine gate stops the enter FIRING, so no
  order is placed on a spread-hour bar to fill).
- `record_last_close` runs BEFORE the `fire_rule` suppression so OnClose crosses
  on the next clean bar aren't desynced.
- `entry_level_vetos` (`is_past`-inclusive, baked on enter) are a SEPARATE path
  (Bug #12) — untouched, so gap-past protection still works during spread hours.
- Exit honouring (SL/TP) in `simulate_fill_windowed` is NOT gated — a real
  broker stops you regardless; the open-position stop-widen covers that case.

## Deferred follow-up (not built)
- A redundant live-clock reject in the worker's `run_enter` (defense-in-depth
  for a manually re-POSTed alert). Skipped: the engine gate already blocks the
  fire in both worker + replay, and a live-clock reject risks double-rejecting a
  legitimately-fired late-arriving enter. Add only if a concrete gap appears.
