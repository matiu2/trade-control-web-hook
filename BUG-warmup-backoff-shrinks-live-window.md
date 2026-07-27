# Bug: the warm-up back-off loop silently SHRINKS the live window

**Severity:** high — replays of the same plan on the same candles score
differently depending on `--start` / `--warmup-bars`, because the two runs
evaluate **different live windows**.

**One line:** widening the warm-up look-back (`pull_from` moves earlier,
`pull_end` unchanged) sometimes returns **fewer live candles**, and
`pull_with_warmup` accepts that silently.

---

## Evidence

Coffee M15, plan `ihs-coffee-a40c79a7`, `--end 2026-07-23T23:59`,
`RUST_LOG=info` (only `--start` differs):

```
--start 2026-07-19T12:00
  attempt 0 -> count=195  warmup=42   live=153
  attempt 1 -> count=455  warmup=302  live=153      <- live STABLE
  => 8 golden, Net R -3.00

--start 2026-07-20T06:00
  attempt 0 -> count=153  warmup=0    live=153
  attempt 1 -> count=279  warmup=126  live=153
  attempt 2 -> count=320  warmup=185  live=135      <- live DROPS 153 -> 135
  attempt 3 -> count=367  warmup=232  live=135
  => 5 golden, Net R -0.40
```

`live` is `candles.len() - warmup_count` where `warmup_count` counts
`c.time < start`. The back-off only ever moves `pull_from` **earlier**;
`pull_end` is fixed. So the live half of the window MUST be invariant —
losing 18 tail bars between attempt 1 and attempt 2 is the bug.

Because the live window differs, the detector sees different bars, so the
golden count and every downstream entry differ. That is the whole
`-0.40R` vs `-3.00R` "start-cursor lottery".

## Why it looked like a cursor bug

`--start` and `--warmup-bars` both change **how many back-off attempts
run**, and it's the attempt count that determines whether you land on a
shrunken window. Doubling `--warmup-bars` to 400 INVERTS the two cursors'
results (07-19 -> -0.40R, 07-20 -> -3.00R) — same plan, same candles.
Runs are otherwise fully deterministic (3x identical invocations agree).

## Ruled out (each tested)

- `record_origin_open` / prep stamps — the -3.00R and -0.40R runs stamp
  b&c 22:15 and retest 22:30 identically, and entry #1 is byte-identical.
- ATR starvation — `detector_lookback_bars` = 98 on M15, warm-up >= 126.
- Wilder-RMA path dependence — measured identical to 5dp across 200/1000+
  bar prefixes; both cursors give ATR 2.49059 at the divergence bar.
- `confirmed_setup_floor` / `plan.replay_start` — untouched by
  `replay-candles --start`; identical across runs.
- The `as_of` bar-close leak (fixed d4a12e7) — all runs are WITH that fix.
- Non-determinism — 3 identical runs all give -3.00R.
- Granularity defaulting to H1 — granularity is threaded correctly
  (`pine_defaults(plan.granularity)`); no `Default` impl exists to fall
  back to. M15 matters only because its 96-bar ATR (vs H1's 24) makes the
  50h wall-clock warm-up span land inside Coffee's 15h session gap, which
  is what makes the back-off loop run at all.

## CONFIRMED in isolation: a RAGGED (non-bar-aligned) `from` loses tail bars

Reproduced with `cli/examples/cache_range_tail_probe.rs` — pure
`CacheClient::get_candles_range_bid_ask` calls, no replay logic, same `to`
every time:

```
from=2026-07-17 18:00:00Z  (aligned)  total=152  warmup=  0  live=152  baseline
from=2026-07-15 03:30:00Z  (aligned)  total=278  warmup=126  live=152  identical
from=2026-07-11 16:54:38Z  (RAGGED)   total=319  warmup=185  live=134  *** -18 ***
from=2026-07-09 14:54:38Z  (RAGGED)   total=366  warmup=232  live=134  *** -18 ***
```

**The discriminator is `from`'s alignment, not its distance.** A `from` on a
bar boundary (`:00`, `:30`) is safe at any depth; a ragged `from`
(`16:54:38`) silently drops 18 bars from the FAR END of the range. Two
earlier probe runs using only whole hours passed cleanly and briefly made
this look like a non-bug — align your probe timestamps or you will not see
it.

Ragged `from` values come from `next_pull_from`'s density extrapolation
(`cli/src/bin/replay_candles.rs`), which computes an instant from a
bars-per-second estimate and never snaps it to the granularity grid. So the
back-off loop's attempt 0/1 (naive `start - bar_secs * want`) are aligned
and safe, while attempt 2+ (extrapolated) are ragged and lossy — exactly
matching the observed `live=153, 153, 135, 135`.

Fix candidates, in order:
1. **candle-cache**: a range query must not let `from`'s sub-bar offset
   affect which bars near `to` are returned. This is the real defect and it
   affects every caller, not just replay.
2. **`next_pull_from`**: snap the returned instant down to the granularity
   grid. Cheap, and removes the trigger for this caller.
3. **`pull_with_warmup` guard**: never accept a live count lower than a
   previous attempt's — fail loudly instead of scoring it.

## Fixes (independent, do both)

1. **candle-cache**: find why a widened range drops tail candles.
2. **replay guard (defence in depth)**: `pull_with_warmup` must assert the
   live count never decreases across attempts — keep the best-live result,
   or fail loudly. Silently accepting a shrunken live window is what turned
   a cache bug into a scoring bug. Matches the no-silent-degrade rule.

## Regression test

Replay one plan from two cursors that differ only in back-off attempt count
and assert identical Net R (and identical live-bar count).
