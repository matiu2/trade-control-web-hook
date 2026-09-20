# BUG — the TradeNation adapter hands the engine the still-FORMING bar on every native granularity

**Status:** FIXED 2026-09-21 — `broker-tradenation-adapter`, this repo. No dependency bump.
**Severity:** CRITICAL — on-close rules fire against bars that have not closed, and the
fire's recorded bar (and the watermark it advances) is a bar that does not exist yet.
**Scope:** **TradeNation only**, granularities **M1 / M15 / H1** (and D1 via the same
native route). OANDA is unaffected. Both the mid and the bid/ask candle paths were affected.
**Found:** 2026-09-21, journalling AUD/NZD H1 H&S (plan `hs-aud-nzd-ff8e66e8`) — surfaced as a
phantom "Δ timing" line in the journal's Compare screen.

## This is the un-fixed half of a bug the repo believes it closed

`BUG-tn-h4-aggregator-emits-incomplete-bucket.md` diagnosed exactly this root cause on
2026-09-08 and was marked FIXED. That fix landed in `tradenation-api::aggregate_candles`,
which is reached **only** when `aggregation::resolve_native` returns `Some` — and it returns
**`None` for M1/M15/H1**, because those have a native TN endpoint. So the native granularities
never reached the fix and kept the bug for another two weeks. The H4 doc's own scope note says
as much ("`resolve_native` returns `None` for H1/M15/D1, so they never reach
`aggregate_candles`"); it was read as "those are safe", when it actually meant "those are
unprotected".

## Symptom

A rule fires one bar EARLIER in the offline replay than in the live worker, with the live
fire stamped on a bar whose recorded OHLC does not match the market's final bar:

| | bar (BNE) | recorded close | market's real close |
|---|---|---|---|
| live `01-veto-too-high`  | 2026-09-07 **12:00** (02:00Z) | 1.22726 | **1.22636** |
| replay `01-veto-too-high`| 2026-09-07 **11:00** (01:00Z) | 1.22739 | 1.22739 ✓ |

The cron ticked at `02:00:21Z` — 21 seconds into the 02:00Z bar. The live record's
`o`/`h` match the real 02:00Z bar (1.2274 / 1.22741) but `l`/`c` do not
(recorded `l == c == 1.22726`, the running last tick): the signature of a bar caught
mid-formation. Replay used the closed 01:00Z bar and matched the broker exactly.

## Cause

`Broker::get_candles`'s contract (`core/src/broker.rs`) is "**Closed only** — the
still-forming current bar is dropped", and `filter_new_candles`
(`core/src/broker/candles.rs`) states that "broker impls call this after dropping any
still-forming bar". It cannot do the dropping itself: it filters `time > watermark`, and a bar
that opened one second ago is legitimately newer than the watermark.

* **OANDA honours it** — its feed carries an authoritative `complete` flag, and
  `broker-oanda/src/candles.rs` filters `.filter(|c| c.raw.complete)` on both paths.
* **TradeNation did not.** `charts.finsatechnology.com` is count-back-from-`end_time` and
  carries **no `complete` flag**, so its newest row is *always* the bar currently open. The
  adapter mapped the rows straight through: `get_candles` handed them to `filter_new_candles`
  (which cannot remove the forming bar) and `get_bidask_candles` used a hand-rolled
  `time <= since` skip that only trims the OLD end.

The engine is blameless: `evaluate_plan` treats every candle it is given as closed, and
`push_fire` stamps the fire with that candle verbatim.

## Why this is worse than a one-bar timing offset

The cron re-runs every **5 seconds** (`worker/src/config.rs`, `default_engine_secs`). So an
`on_close` rule on a TN native-TF plan got ~720 chances per H1 bar to fire against a
*provisional* close. It therefore fires on the **first transient touch inside the bar**, which
the bar's real close may never confirm — a false fire, not merely an early one. In the
incident the 02:00Z bar's running close (1.22726) was above the `too-high` level while its
actual close (1.22636) was **below** it. The veto also advanced the watermark past a bar that
had not closed.

## Fix

`broker-tradenation-adapter/src/lib.rs`: `drop_forming_bar(candles, granularity, as_of)` keeps
only bars with `open + granularity <= as_of`, applied on **both** candle paths through two
extracted, unit-testable transforms — `closed_candles_since` (mid) and `closed_bidask_window`
(bid/ask). Generic over a small `BarOpen` trait so there is ONE predicate, not a second copy
the bid/ask path could keep the bug with.

Done in the adapter rather than in `tradenation-api` so it covers **every** granularity
(native and aggregated) in one place, and needs no bump of the pinned git dependency.

Boundary: `<=`, so a bar is kept the instant it closes. A strict `<` would withhold the
just-closed bar for a whole extra cron tick and then lose it once the watermark moved past.

## Testing note — the pure tests were NOT enough

Tests of `drop_forming_bar` alone all passed **with the call deleted from both call sites**
(verified by mutation). They proved the helper worked, not that anything used it. That is why
the post-fetch transforms are extracted: the tests now drive the call site, and unwiring either
path turns them red. Mutations run and caught: mid drop removed; bid/ask drop removed; `<=`
→ `<`; sort removed. One mutation (swapping the drop and the watermark trim) **survived and
should** — both are pure filters, so they commute; the doc comment that claimed the order was
load-bearing was wrong and has been corrected.

## Follow-up

* **Fixtures are unaffected** — the offline replay pulls through candle-cache, whose
  `markable_buckets` already enforces `start + bar <= as_of`. Replay was always right; this
  brings live up to it. No re-bless needed.
* Historical TN timelines recorded before this fix still carry forming-bar stamps, so the
  journal's Compare screen will keep showing a one-bar Δ for plans that ran before 2026-09-21.
  That is a true record of what live did, not a display bug.
* `SCOPING-shared-candle-cache-puller.md` blocker #2 (the `complete` flag is lost through
  `candle_model::CandleData`) is the deeper fix — one puller for live and replay. This closes
  the live-side symptom without waiting for it.
