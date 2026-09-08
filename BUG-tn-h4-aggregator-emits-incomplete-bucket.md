# BUG — the TradeNation H4 aggregator emits each bucket 1h EARLY, built from a still-forming H1 bar

**Status:** FIXED 2026-09-08 — `tradenation-api` `bcd219c` (tag `broker-tradenation-v0.17.0`),
`candle-cache` `0512b33`. Pins bumped in this repo; fixtures NOT yet regenerated (see below).
The diagnosis in this doc was **correct** and is what the fix implements.
**Severity:** CRITICAL — on-close rules fire against bars that have not closed; positions are
closed on prices that never became a close. Live and replay score different trades.
**Scope:** **TradeNation only.** OANDA is unaffected (it filters `complete == false`).
Affects every TN H4 and M5 plan.
**Found:** 2026-09-08, journalling GBP/NZD H4 iH&S (demo trade 153, plan `ihs-gbp-nzd-9e63a168`)

## Symptom

On an H4 plan whose bars close at 03/07/11/15/19/23 Brisbane, every live bar-triggered fire lands
**one hour before a grid slot**, and one position-closing veto fires a **full day earlier** than
the replay reaches it:

| rule | live fire (BNE) | replay bar (BNE) |
|---|---|---|
| 03-prep-break-and-close | 2026-08-12 **18:00** | 2026-08-12 15:00 |
| 01-veto-too-high        | 2026-08-12 **22:00** | 2026-08-12 19:00 |
| 07-close-on-sr-reversal | 2026-08-14 **06:00** | 2026-08-14 03:00 |
| 01-veto-too-low         | 2026-08-20 **14:00** | 2026-08-21 15:00 |

## Cause

TradeNation has no native H4 endpoint, so the adapter aggregates 4x H1
(`broker-tradenation-adapter/src/lib.rs:693` `TN_SERVES_H4 = true`; `:700-709` routes H4 to the
aggregating path).

`tradenation-api/src/aggregation.rs:86-102` emits a bucket as soon as `group.len() >= multiplier`
(4) and takes `close = group.last().close`. The raw feed
(`tradenation-api/src/ohlcv.rs:65-105`) carries **no `complete` flag and drops no partial bar** —
its newest H1 row is always the still-forming one. The adapter adds no completeness filter either
(`lib.rs:296-326`), unlike OANDA (`broker-oanda/src/candles.rs:60`, `:115` —
`.filter(|c| c.raw.complete) // drop the still-forming bar`).

The bucket **grid** is correct — `tradenation-api/src/session_anchor.rs:36-82` anchors H4 to 17:00
America/New_York (DST-aware) = Brisbane 03/07/11/15/19/23, matching replay. The defect is *when*
the bucket is emitted. For the bucket opening BNE 15:00 (05:00Z), its four H1 bars are stamped
05, 06, 07, **08**Z. The 08:00Z bar exists at 08:00:00Z = **BNE 18:00**, one hour before the
bucket's true close (09:00Z = BNE 19:00). `len()` hits 4 there and the bucket ships.

Reproduced arithmetically for every slot:

    bucket opens BNE 11:00 (01Z) | 4th H1 bar 04Z -> emitted BNE 14:00 | true close BNE 15:00
    bucket opens BNE 15:00 (05Z) | 4th H1 bar 08Z -> emitted BNE 18:00 | true close BNE 19:00
    bucket opens BNE 19:00 (09Z) | 4th H1 bar 12Z -> emitted BNE 22:00 | true close BNE 23:00
    bucket opens BNE 03:00 (17Z) | 4th H1 bar 20Z -> emitted BNE 06:00 | true close BNE 07:00

All four live fire times above appear in the middle column. Verified against the live feed: at
02:57Z the bucket opening 01:00Z had only 2 bars (not emitted) while the 21:00Z bucket had 4.

**The scheduler is not involved.** `worker/src/scheduler.rs:94-139` is a plain 5-second interval
(`worker/src/config.rs:136-138`), with no hour offset or bar-boundary arithmetic. A live fire
stamp is `RequestRecord.ts` = when the request was received (`core/src/recording.rs:74-75`), i.e.
the wall clock at which the engine first *saw* the bar. This is also **not** an open-vs-close
stamping difference — "+3h after the replay's bar-open stamp" and "1h before the next grid slot"
are the same instant described two ways.

## Why `too-low` fired a day early — same cause, and the dangerous one

Both sides run the same `level_crossed` on close (`engine/src/evaluate.rs:1857`, `:2280-2291`).
There is no separate live tick-evaluation path. The divergence is in the **data**.

The bucket opening **BNE 2026-08-20 11:00** (emitted live at BNE 14:00), from real GBP/NZD H1:

    BNE 11:00  C=2.28836
    BNE 12:00  C=2.28692
    BNE 13:00  C=2.28714
    BNE 14:00  O=2.28713  H=2.28739  L=2.28531  C=2.28658   <- still forming

Level = **2.285674661976239**. The true H4 close is **2.28658, above** the level — replay
correctly did not fire, and only fired on the 08-21 15:00 bar (close 2.28548). But the
still-forming 14:00 H1 bar's **low is 2.28531, below the level**. Its running close dipped under
the level during 14:00-15:00, the aggregator handed the engine an H4 bar carrying that close, and
`too-low` (dir `down`, `bar: on_close`) saw a legitimate downward close-cross and fired —
**closing positions on a bar that never closed there.**

Because the bucket is re-read every 5s until it completes, every H4 OHLC value live sees is
provisional: the emitted high/low span only the partial 4th hour.

Replay is immune — `cli/src/bin/replay_candles/candles.rs:45-63` pulls a historical window in
which every H1 bar is fully formed.

## Prior art — this is a regression from a previous fix

`BUG-replay-vs-live-3.3-divergences.md` ("3.3 BUG #2") is the direct ancestor: TN H4 was
originally un-feedable live, and the chosen fix was "aggregate H4/M5 from native TFs". That fix
landed (`TN_SERVES_H4 = true`) but **shipped without a completeness filter**, converting a
"never fires" bug into a "fires an hour early on a provisional bar" bug. Its own closing note —
*"replay must mirror it… else the dataset keeps lying"* — is precisely what broke.

That same file already flags, under the eur-zar/nzd-usd item, *"a ~1h bar-alignment shift, which
moves a protective too-high veto one bar relative to the entry"* — an earlier, unexplained
sighting of this same shift.

## Fix — SHIPPED

As proposed here, but with one change: the member count was **removed**, not ANDed with a
time condition. A bucket is now emitted once its window has **elapsed** —
`bucket_start + target_secs <= as_of` — regardless of member count.
`aggregate_candles` takes an `as_of: DateTime<Utc>` in place of `multiplier`.

**Why the count arm had to go rather than be tightened.** Keeping
`group.len() >= multiplier && elapsed` would fix this bug but leave
`BUG-fixture-drops-monday-open-candle.md` permanently broken: the FX week-open H4 bucket
can only ever hold **three** H1 bars (the week opens 22:00 UTC Sunday but the 17:00-NY
grid starts that bucket at 21:00 UTC), so it fails the count arm no matter what the clock
says. A member count cannot separate a legitimately short bucket from a still-forming one
— they are identical by count — which is why the old rule was wrong in *both* directions.
One predicate now fixes both bugs. **Do not reintroduce a member count.**

The `cutoff` design proposed here was adopted: `aggregate_candles` stays pure over its
slice, and `get_candles_range_aggregated` threads its existing `end_time` through rather
than calling `Utc::now()` internally — using the wall clock for a historical range would
emit buckets past the requested window. `get_candles_aggregated` (the "latest N" path)
passes `Utc::now()`, which is the signature ripple this doc predicted; 9 call sites.

Pinned by `aggregate_withholds_a_full_bucket_that_has_not_closed`, which asserts a bucket
holding **all** its members still does not ship while its window is open. Mutation-tested:
restoring `len() >= multiplier` fails 5 tests in `tradenation-api` and 3 in `candle-cache`.
`aggregate_drops_incomplete_group`, which pinned the old wrong behaviour, was **replaced,
not deleted**.

`candle-cache/src/aggregation.rs` carried the same latent defect on the mid path and was
fixed identically.

**Fixtures regenerated 2026-09-08** (`b7e277b`): all **48** TN H4 cells re-fetched from
their frozen `.spec.json` — a single regeneration covering both bugs, as planned. Note
`--rebless` alone would have been a **no-op**: `--test-mode` replays the fixture's own
frozen candles, so it re-scores stale bars rather than re-pulling from the broker.
Scope was provably 48 cells (`resolve_native` returns `None` for H1/M15/D1, so they never
reach `aggregate_candles`; verified by diffing every dir against a pre-run backup).
Both halves of the fix are visible in the diff: bars **added** inside the window are the
week-open buckets, bars **removed** are all past `meta.end` — this bug's still-forming
buckets. Gate: 918/918 real cells pass, `cargo test -p trade-control-cli` 304/304.

**Still to do:** `entry-rule-corpus-comparison.md` (the basis of the current `skip-bcr`
entry rule) must be re-run — the corpus is now correct, so this is unblocked.

## Impact on the journal

Every TN H4/M5 trade's live fire times are one bar early, and any on-close rule may have fired on
a price that was never a close. Position-closing vetos (`too-low`, `too-high`,
`close-on-sr-reversal`) are the serious case — they can flatten a trade on a phantom cross, as
happened here. Live-vs-as-designed comparisons across the journal inherit this.

## Repro

    replay-candles-staging --test-mode --fixtures-glob 'gbp-nzd-h4-2026-08-05-*' --verbose

Compare bar times against the live `plan-timeline` for `ihs-gbp-nzd-9e63a168` (table above). Plan
confirmed TradeNation on every axis: all rule intents carry `broker: tradenation`, fixture
`meta.source = tradenation`, `candle_source = tradenation`, `chart_symbol = TRADENATION:GBPNZD`.
