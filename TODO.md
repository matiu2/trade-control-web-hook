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
- [x] Deployed staging → catch-up tick completed (GBP/USD advanced
      await_break_and_close → await_entry), no re-panic. DONE (committed 04042ab).

## Follow-up
- Update CLAUDE.md "retest closeness decays over time" note: the ATR
  "hard-fails — structurally unreachable" claim is what bit us. (done)

---

# TODO 2 — spread-hour suppression only on ≤1h bars (15m/1h), not H4/D

## Rationale (operator)
"We don't need to suppress 7am entries on the 4h chart, only on the 15m and 1h
charts. The other 3 hours in the 4h candle balance the rubbish with real data."
We only trade 15m/1h/4h/D. So: suppress on 15m+1h, allow on 4h+D.

## Change
- [x] `core`: new `suppress_on_spread_hour(instr, now, granularity)` +
  `_bar_seconds` twin = `is_spread_hour AND bar ≤ 3600s`. Single policy seam.
  `stop-widen`/pending-lifecycle consumers of `is_spread_hour` deliberately
  NOT gated (they protect an open stop through the spike, any bar size).
- [x] `engine/evaluate.rs`: all 4 rubbish-candle suppression sites
  (entry/signal, retest stamp, intrabar veto/cross via `fire_rule`,
  reversal-close detection) now call `suppress_on_spread_hour(plan.granularity)`.
  `fire_rule`/`control_rule_fires` gained a `granularity` param.
- [x] `engine/simulator.rs`: `find_fill` spread-hour skip gated on bar-seconds
  derived from candle spacing (`bar_seconds_of`) so replay fill == live.
- [x] `cli/replay.rs`: the "spread-hour suppressed" trace flag gated too, so an
  H4 bar no longer shows a false "not entering" line.

## Tests
- [x] core `suppression_only_applies_to_short_bars`,
  `suppression_bar_seconds_matches_the_granularity_seam`.
- [x] engine `spread_hour_does_not_suppress_an_h4_entry` (+ existing H1
  suppress/clean-twin tests unchanged → 149 pass).

## Verify
- [x] core 852 / engine 149 / cron 11 pass; worker + replay-candles build;
  clippy clean (pre-existing `for h in 0..24` warning untouched).
- [ ] Deploy staging; on the next GBP/USD (H4) replay the `07-14 07:00
      spread-hour suppressed` line should be GONE and the entry taken.
