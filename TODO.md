# TODO — fix retest_tolerance panic that kills the cron loop

## Incident
Staging engine cron loop panicked 2026-07-14 04:32:56 UTC at
`engine/src/evaluate.rs:1388` (`retest_tolerance` `.expect()` on a `None` ATR
from a too-short detector window). The panic unwound the whole `tc-scheduler`
thread (all 7 cron loops share one `tokio::join!`), so the engine tick,
break-even watcher, blackout watchers, sweep and GC all died at once. The axum
HTTP thread survived → the outage was invisible (`/health`/`status` still ok)
and every plan's watermark froze ~17.5h.

## Fixes (both needed — defense in depth)

- [x] **1. `retest_tolerance` must not panic.** On `wilder_atr == None`
  returns `0.0` tolerance (strict must-reach) + `warn!`. `engine/src/evaluate.rs`.
  - [x] Test `retest_tolerance_short_window_degrades_to_zero_no_panic`.
  - [x] Existing `retest_tolerance_grows_linearly` still passes (warm path unchanged).

- [x] **2. `engine_tick_loop` panic isolation.** `run_isolated()` helper spawns
  the tick as a `spawn_local` task and inspects the `JoinHandle` for a panic —
  contained + logged, loop continues. `worker/src/scheduler.rs`.

## Verify
- [x] `cd engine && cargo test` → 148 pass; clippy clean; fmt run.
- [x] `cd trade-control-cron && cargo test` → 11 pass; worker crate builds; clippy clean.
- [ ] Deploy staging → confirm the catch-up tick completes (watermarks advance
      past 07-14 04:00) and no re-panic.

## Follow-up
- Update CLAUDE.md "retest closeness decays over time" note: the ATR
  "hard-fails — structurally unreachable" claim is what bit us.
- Then resume the spread-hour hunt on the now-healthy worker.
