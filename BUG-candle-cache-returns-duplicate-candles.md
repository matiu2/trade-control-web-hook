# Bug: candle-cache returns DUPLICATE candles, so replays score differently

> **FIXED 2026-07-27 — `candle-cache` v3 (`8244a67`).** `merge::sort_and_dedup`
> now collapses each merged series to one bar per timestamp BEFORE the
> count-based trim, at all four `client.rs` merge sites; `aggregation.rs` dedups
> within each bucket at grouping time so the completeness gate counts distinct
> bars. Verified: all four probe calls report 134 distinct bars / 0 duplicates,
> and **all four replay cursors now agree at Net R −0.40** (was −0.40 vs −3.00).
> Remaining follow-up: the loud guard in `pull_with_warmup` (item 2 below).

**Severity:** high — replays of the same plan on the same candles score
-0.40R or -3.00R depending on `--start` / `--warmup-bars`, because some
range fetches feed the engine **duplicated bars**.

**One line:** `candle-cache` merges cached + freshly-fetched chunks with
`sort_by_key` and **no dedup**, then truncates by COUNT — so duplicates both
corrupt the series and push real bars off the end. `pull_with_warmup`
accepts the result silently.

> **Reading note:** the investigation reversed itself twice. Sections below
> are in discovery order; the CONFIRMED section is authoritative. Early
> framing ("widening loses tail bars") is **wrong** — it was a duplicate
> count, not a loss.

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
`pull_end` is fixed, so the live half MUST be invariant. (Superseded
framing: the 153 is the *inflated* number — 135 is the true bar count. See
CONFIRMED below.)

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

**CORRECTED — the ragged call is the CORRECT one.** Counting distinct
timestamps inverts the reading:

```
from=2026-07-17 18:00:00Z (aligned)  live=152  distinct=134  DUPES=18
from=2026-07-15 03:30:00Z (aligned)  live=152  distinct=134  DUPES=18
from=2026-07-11 16:54:38Z (RAGGED)   live=134  distinct=134  DUPES= 0
from=2026-07-09 14:54:38Z (RAGGED)   live=134  distinct=134  DUPES= 0
```

All four calls return the **same 134 real bars**, same first/last bar, same
session gaps. The ALIGNED calls **duplicate 18 of them**. So the bug is not
"widening loses bars" — it is **"an aligned `from` emits duplicate
candles"**, and the replay's `live=153` was an inflated count while
`live=135` was honest. Deterministic: 3 identical runs agree exactly (not a
cache-population race).

**Root cause — `candle-cache` merges without dedup.**
`CacheClient::fill_cache_gaps` (`candle-cache/src/client.rs` ~645) does:

```rust
all_candles.extend(cached_range_entries);   // cached chunks
all_candles.extend(fetched_candles);        // freshly-fetched chunks
all_candles.sort_by_key(|c| c.timestamp()); // sorted...
// ...but never deduped
.take(requested_range.candle_count())       // truncates by COUNT
```

Overlapping cached/fetched chunks yield duplicate timestamps, and the
count-based `take` then pushes real bars off the end. `get_candles_range`
(~635) has the same sort-then-`truncate` with no dedup. There is **no
`dedup`/`dedup_by_key` anywhere in the crate** — only `sort_by_key`.

Whether a given `from` produces overlapping chunks depends on how it lands
against the cached-range boundaries, which is why alignment (not distance)
is the discriminator.

### Checked and NOT the cause: TN synthetic/aggregated candles

Worth ruling out explicitly, because TradeNation *does* synthesise some TFs
and chunked fetches *do* deliberately overlap:

- **Coffee M15 is TN-NATIVE**, not synthesised. `to_cm_granularity`
  (`broker-tradenation-adapter/src/lib.rs:579`) maps M1/M15/H1/D1 to native
  endpoints; only **H4 (=4xH1) and M5 (=5xM1)** are adapter-aggregated. So
  no aggregation runs on this path at all.
- **Chunked count-back requests overlap on purpose.** `chunk_windows`
  (~620) gives each chunk `+1+SLACK` bars so adjacent chunks overlap rather
  than leave a seam gap — a deliberate duplicate source. But the adapter
  **dedups it correctly**: `get_bidask_candles` unions chunks through a
  `HashMap<DateTime<Utc>, BidAskCandle>` (~322) so the seam collapses to one
  bar.
- `candle-cache`'s own `aggregation.rs` groups via a `HashMap` keyed on the
  aligned bucket, so it cannot duplicate a bucket within one call — though
  note it would happily aggregate *duplicated inputs* into a wrong OHLC
  (`group_candles.len() >= multiplier` would pass spuriously). Not triggered
  here, but a real hazard for H4/M5 once duplicates exist upstream.

So the duplicates are introduced **downstream of the adapter, in
candle-cache's merge** — the sites named above. The adapter shows the right
pattern to copy.

Ragged `from` values come from `next_pull_from`'s density extrapolation
(`cli/src/bin/replay_candles.rs`), which never snaps to the granularity
grid — which is why attempts 0/1 (aligned, duplicated) and attempt 2+
(ragged, clean) differ, matching `live=153, 153, 135, 135`.

Fix, in order:
1. **candle-cache (the real defect)**: dedup by timestamp after every merge,
   BEFORE the count-based `take`/`truncate`, in `fill_cache_gaps` and
   `get_candles_range` (and the two bid/ask twins ~950/1009). Affects every
   caller, not just replay — duplicated bars are fed to any indicator built
   on a range fetch.
2. **`pull_with_warmup` guard**: never accept a live count lower than a
   previous attempt's, and assert distinct timestamps — fail loudly instead
   of silently scoring a duplicated window
   ([[no_silent_degrade_prefer_loud_failure]]).
3. **`next_pull_from`** (optional): snapping to the granularity grid changes
   which `from` values occur but does NOT fix the dedup bug — do not treat
   it as a fix.

## Regression tests

- **candle-cache**: a range fetch must return strictly-increasing distinct
  timestamps for any `from`, aligned or ragged. `cli/examples/
  cache_range_tail_probe.rs` is the manual version; port it to a unit test
  in the crate.
- **replay**: replay one plan from two cursors that differ only in back-off
  attempt count and assert identical Net R and identical live-bar count.
