# BUG — the candle fetch drops the Monday-open (21:00 UTC Sun) H4 bar, every week

**Status:** FIXED 2026-09-08 — `tradenation-api` `bcd219c` (tag `broker-tradenation-v0.17.0`),
`candle-cache` `0512b33`. Pins bumped in this repo; fixtures NOT yet regenerated (see Follow-up).
**Severity:** HIGH — replays silently score a different trade than the chart shows; journal R is wrong
**Found:** 2026-09-08, journalling GBP/NZD H4 iH&S (demo trade 153)

> ## ⚠️ The original "Cause — FOUND" section below was WRONG
>
> It named `core/src/intent/blackout/baked.rs:44` (`WEEKEND_TO_MIN`). That constant is
> **not in any candle path** — its only consumers are the entry gates in
> `core/src/dispatch/enter.rs:516` and `core/src/sweep_gate.rs:97`. Changing it would
> not have restored a single bar; it would only have loosened a live entry guard.
>
> The real cause is the member-count completeness gate in the TradeNation H4
> aggregator — see **"Cause — ACTUAL"** below. The wrong section is kept for the
> record because it misled a second agent into ruling out the true cause.

## Symptom

The operator's TradingView chart (with the Candle Signals study) marks a golden bar at
**2026-08-10 07:00 Brisbane**. No replay mentions that bar — not as golden, not as a decline, not
at all — at any `--start`, including starts 24h and 6 days earlier.

The bar is not being rejected. **It is missing from the candle data.**

## Evidence

Live TradingView (`tv-mcp ohlcv`, TRADENATION:GBPNZD, 240) vs the replay fixture, same window:

    TradingView (live)            Fixture (all 8 variants)
    2026-08-08 03:00              2026-08-08 03:00
    2026-08-10 07:00  <-- EXISTS  ..............  <-- MISSING
    2026-08-10 11:00              2026-08-10 11:00

The missing bar: **O 2.29053  H 2.29364  L 2.28671  C 2.29249** — a 69.3-pip range bar, the
largest in that neighbourhood, which is why the Candle Signals study marks it.

Diffing the whole fixture window against TradingView, the dropped bars are **exactly the Monday
07:00 bars, every week, with no exceptions**:

    2026-07-27 07:00 Mon
    2026-08-03 07:00 Mon
    2026-08-10 07:00 Mon
    2026-08-17 07:00 Mon
    2026-08-24 07:00 Mon

(Everything else flagged by the diff is simply past the fixture's `end`.) Every *other* Monday slot
— 11:00, 15:00, 19:00, 23:00 — is present. Only the week's first bar is dropped.

`2026-08-10 07:00` Brisbane = `2026-08-09 21:00 UTC` = **Sunday 21:00 UTC**, the FX week open.

Enumerating every Sunday-21:00-UTC bar in the live TradingView feed shows these are consistently
LARGE bars — they span the weekend gap, so they carry the Friday-close-to-Monday-open move:

    utc 2026-07-26 21:00 Sun -> BNE 2026-07-27 07:00 Mon   range 65.6p
    utc 2026-08-02 21:00 Sun -> BNE 2026-08-03 07:00 Mon   range 54.7p
    utc 2026-08-09 21:00 Sun -> BNE 2026-08-10 07:00 Mon   range 69.3p   <-- the disputed bar
    utc 2026-08-16 21:00 Sun -> BNE 2026-08-17 07:00 Mon   range 67.9p
    utc 2026-08-23 21:00 Sun -> BNE 2026-08-24 07:00 Mon   range 62.6p

Against an ATR of ~46p, every one of these clears 1x ATR. **The dropped bar is systematically the
kind of bar most likely to qualify as a golden signal** — which is the worst possible bar to lose
for a reversal strategy gated on golden candles.

## Cause — SUPERSEDED (wrong; kept for the record)

`core/src/intent/blackout/baked.rs:41-44`, the universal weekend blackout:

```rust
/// UTC minute-of-day entry resumes on Sunday — the FX week reopens ~21:00-22:00
/// UTC Sunday (Monday morning Sydney/Tokyo). We reopen at 22:00 UTC.
const WEEKEND_TO_MIN: u32 = 22 * 60;
```

The weekend halt is defined as Friday 21:00 UTC (`WEEKEND_FROM_MIN = 21 * 60`, line 38) through
**Sunday 22:00 UTC**. But the FX week-open H4 bar OPENS at **Sunday 21:00 UTC**. The blackout
therefore covers that bar's entire span, and it is excluded.

The comment itself concedes the ambiguity — "the FX week reopens ~21:00-22:00 UTC Sunday" — and
then picks the later end of the range. That one-hour overshoot is the bug: TradingView's feed
(and the broker's own H4 grid) starts the week at 21:00 UTC, so `WEEKEND_TO_MIN` should be
`21 * 60` to match the first bar of the week, not `22 * 60`.

Note this is a *blackout* boundary, i.e. its original purpose is to stop entries resting into a
halted market. Using the same constant to bound the candle series is what turns a conservative
entry guard into silent data loss. Worth checking whether the two uses should share a constant at
all — the safe entry-resume time and the first valid bar of the week are different questions.

It is not a timezone issue — every other bar in the grid aligns exactly, and all nine of the
plan's pause/news epochs decode to the correct Brisbane wall-clock.

## Cause — ACTUAL

`tradenation-api/src/aggregation.rs:88` (pre-fix):

```rust
.filter(|(_, group)| group.len() >= multiplier)
```

with the function's own doc conceding it: *"Incomplete groups (fewer than `multiplier`
candles) are dropped."*

TradeNation has no native H4, so H4 is aggregated from 4x H1
(`broker-tradenation-adapter/src/lib.rs`, `TN_SERVES_H4 = true`). The FX week opens
**22:00 UTC Sunday** (17:00 NY), but in summer (EDT) the 17:00-NY H4 grid starts that
bucket at **21:00 UTC** — so it can hold at most **three** H1 bars before the boundary
and `len() >= 4` is never satisfied. Pulled live from the feed:

    2026-08-07 20:00 UTC Fri   <- last bar of the week
            ...gap...
    2026-08-09 21:00 UTC Sun   O 2.29053 H 2.29364 L 2.28671 C 2.28869
    2026-08-09 22:00 UTC Sun
    2026-08-09 23:00 UTC Sun
    2026-08-10 00:00 UTC Mon

The bucket grid itself is **correct** — `session_anchor.rs` anchors H4 to 17:00
America/New_York, DST-aware, matching TradingView. Only the emit decision was wrong.

### Why the "only the week's FIRST bar" pattern misled us

The evidence section above lists **Brisbane** times, which disguises the shape:

    07:00 BNE Mon = 21:00 UTC SUNDAY   <- dropped
    11:00 BNE Mon = 01:00 UTC Mon      <- present
    15:00 BNE Mon = 05:00 UTC Mon      <- present

The dropped bar is not "the first Monday bar" — it is the **Sunday 21:00 UTC**
weekend-edge bucket, and there is no Monday-specific rule needing explanation. Nor is it
alone: the fixture's weekly gap is **56 hours** (Fri 17:00 UTC → Mon 01:00 UTC), not 48,
because the **Friday 21:00 UTC** bucket is missing too (0 members — correctly absent,
not a bug). Two weekend-edge buckets, both short on members: exactly a count shortfall.

This pattern ("only the week's first bar, so a count shortfall can't explain it") was
used to *rule out* the true cause. Convert to UTC before reasoning about bar grids.

### Same defect, second copy

`candle-cache/src/aggregation.rs:63-73` carried an identical member-count drop. It did
**not** cause the observed fixture drops — `CacheClient::get_candles_range_bid_ask` never
synthesises bid/ask from a smaller granularity, so the TN path bypasses it — but it sits
on the **mid** path and was fixed too.

## Impact — this invalidates replays, not just traces

The Monday-open bar is a real, tradeable, often-large bar (69.3p here vs an ATR of ~46p). Dropping
it means:

1. **Golden signals are missed.** A qualifying entry bar the operator can see on the chart never
   reaches the engine.
2. **Fills and stops are missed.** The bar's 69.3p range is never scanned for touches, so an entry,
   stop or TP that would have triggered inside it is invisible.
3. **ATR is computed from an incomplete series**, skewing every downstream size/floor decision.
4. **Live-vs-replay comparisons are unsound.** Live traded the real feed; replay scored a feed with
   one bar per week removed. Any journal entry whose as-designed R rests on a replay spanning a
   Monday open is suspect.

For trade 153 specifically, replays with different starts already disagree wildly (+0.00R, -1.00R,
+1.36R), and the missing bar sits inside that window — so **no as-designed R for this trade can be
trusted until the fetch is fixed and the fixtures regenerated.**

## Fix — SHIPPED

A bucket is now emitted once its window has **elapsed** — `bucket_start + target_secs <=
as_of` — regardless of member count. `aggregate_candles` takes an `as_of: DateTime<Utc>`
completeness reference in place of `multiplier`; callers pass `Utc::now()` for a "latest
N" fetch and the explicit `end_time` for a historical range (wall-clock there would emit
buckets past the requested window).

This one predicate fixes **both** this bug and
`BUG-tn-h4-aggregator-emits-incomplete-bucket.md`: a short-but-finished bucket emits, a
full-but-unfinished one does not. A member count cannot separate those two cases — a
legitimately short bucket and a still-forming one are identical by count — which is why
the old rule could not be right in either direction. **Do not reintroduce a member count.**

Verified against the **live** feed, not just fixtures: GBP/NZD H4 now returns 10
week-open bars where it previously returned zero, and the disputed bar matches the
operator's TradingView chart exactly on all four values:

    2026-08-09 21:00 UTC   O 2.29053  H 2.29364  L 2.28671  C 2.29249
    operator's TV bar      O 2.29053  H 2.29364  L 2.28671  C 2.29249

(The aggregate spans four members — Sun 21,22,23 plus the Mon 00:00 UTC bar, which is
inside the 21:00→01:00 window. A three-member aggregate would close at 2.29140.)

Mutation-tested: restoring `len() >= multiplier` fails 5 tests in `tradenation-api` and
3 in `candle-cache`. Two tests that pinned the old wrong behaviour were **replaced, not
deleted** (`aggregate_drops_incomplete_group`, and candle-cache's
`duplicates_cannot_fake_a_complete_bucket` — whose valid half, that duplicates must not
inflate volume/OHLC, was kept as its own test).

`WEEKEND_TO_MIN` was **not** changed — see the correction at the top.

## Follow-up

1. ~~**Regenerate the TradeNation H4 fixtures.**~~ **DONE 2026-09-08** (`b7e277b`) — all
   **48** TN H4 cells re-fetched from their frozen `.spec.json`. See "Fixture
   regeneration" below for the scope proof and what moved.
2. **Re-run `entry-rule-corpus-comparison.md`** — **STILL OUTSTANDING.** The current
   `skip-bcr` entry rule was chosen on the affected data, so that conclusion is
   unverified until it is re-run. The corpus is now correct, so this is unblocked.
3. The doc's original suggestion to split `WEEKEND_ENTRY_RESUME_MIN` (22:00) from
   `WEEKEND_DATA_RESUME_MIN` (21:00) still stands **on its own merits** — the entry-safety
   margin and the first-valid-bar are different questions — but it is **not** a fix for
   this bug and must not be described as one.
4. A fetch-time sanity check (assert the returned bar count matches the window's expected
   sessions, warn loudly on a shortfall) would have caught this in April 2026, when the
   member-count gate first shipped.

## Repro

    tv-mcp ohlcv -n 200          # with TRADENATION:GBPNZD @ 240 loaded
    # -> contains 2026-08-10 07:00 +10:00 (O 2.29053 H 2.29364 L 2.28671 C 2.29249)

    python3 -c "import json,datetime; ..."   # same window from
    # replay-fixtures/gbp-nzd-h4-2026-08-05-*/candles.json -> bar absent

## Lesson for diagnosis

This was misdiagnosed twice (as an hour-blacklist, then as a weekend market closure) because the
fixture was used as evidence about itself. **When the data is the suspect, check an independent
source first.** The operator said three times that the chart showed the bar; pulling the chart
would have settled it immediately.

## Fixture regeneration (2026-09-08, `b7e277b`)

**`--rebless` alone would have been a silent no-op.** `--test-mode` replays the
fixture's **own frozen candles**, so re-blessing re-scores the stale bars and
writes back the same numbers. The candles had to be **re-pulled from the broker**
via `tv-arm --spec-in … --save-matrix replay --save`.

**Scope: exactly 48 cells**, and this is proven, not assumed.
`aggregation::resolve_native` returns `None` for H1/M15/D1, so those bypass
`aggregate_candles` entirely — only H4 (and M5, which has no fixtures) can move.
Confirmed empirically by diffing every fixture dir against a pre-run backup: 48
changed, 0 others.

**⚠️ Pass `--instrument` when regenerating.** `replay-candles` resolves the
instrument from the **TradingView chart** before falling back to the plan, so
re-arming several setups in one session silently pulled the chart's last-loaded
symbol for all of them. Caught because EUR/CAD cells showed entries at 2.29
(GBP/NZD prices) against a 1.60 SL..TP band. Every setup must pass its own
`--instrument`; see `[[replay_candles_reads_chart_symbol]]`.

**Per-cell diff had zero unexplained bars across all 48:** every ADDED bar falls
inside `[meta.start, meta.end]` and is a 21:00 UTC week-open bucket (this bug);
every REMOVED bar is strictly after `meta.end` (the sibling bug — buckets emitted
past the requested window, now cut by the `end_time` reference).

28 of 48 cells moved; TN-H4 Net R **+30.48 → +18.49**. Both directions are real:

- **gbp-nzd 2026-08-05** gains the disputed `2026-08-09T21:00Z` bar and now takes
  a trade worth up to **+4.43R** where it scored +0.00R.
- **eur-cad 2026-07-23** *loses* its entries: the restored week-open bars pierce
  the `too-low` level, so the entry-level veto now correctly blocks them. A
  fixture getting *worse* here is the fix working.

Gate: **918/918** real cells pass; `cargo test -p trade-control-cli` **304/304**
green with the corpus complete.
