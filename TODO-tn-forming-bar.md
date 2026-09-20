# TODO — TradeNation hands the engine the still-forming bar (native granularities)

## What this is

Reported as "the journal's Δ timing line is wrong — both sides fire at the bar end, replay
just reports the start". Investigated: **that is not what was happening.** Both sides already
stamp the bar OPEN (`divergence.rs` reads the live `fired[].candle.time`; the replay report
prints `candle.time`), so the conventions already matched. The two sides fired on genuinely
DIFFERENT bars, with different prices — live's was a bar still forming.

Full write-up: [`BUG-tn-native-granularity-emits-forming-bar.md`](BUG-tn-native-granularity-emits-forming-bar.md).

## Tasks

- [x] Reproduce and prove it from data, not inference — fetched the real AUD/NZD H1 bars from
      the broker and matched them against both sides' records.
- [x] Find the root cause: the TN adapter never dropped the still-forming bar, on either the
      mid or the bid/ask path, breaking a contract OANDA honours.
- [x] Establish it is the un-fixed half of `BUG-tn-h4-aggregator-emits-incomplete-bucket.md`
      (that fix only covers the aggregated path; `resolve_native` returns `None` for
      M1/M15/H1, so they never reached it).
- [x] Fix in the adapter — covers native AND aggregated, no dependency bump.
- [x] Tests at the CALL SITE, not just the helper (see the trap below).
- [x] Cross-reference the H4 bug doc so its "FIXED" banner stops misleading.
- [x] clippy + fmt clean.
- [x] Workspace tests green — **3419 passed, 0 failed** across 58 crate targets.
- [x] Verified against the LIVE feed: at 22:20:11Z the TN endpoint returned the
      22:00Z bar (volume 519 vs the prior bar's 1160) — i.e. the bug was active
      at the moment of the fix, and `open + 1h <= as_of` drops exactly that bar.
- [x] Commit + push + advance the parent pointer.
- [ ] Deploy (operator's call — `./deploy-staging.sh` rolls the live demo worker).

## The testing trap this hit

The first round of tests exercised the pure `drop_forming_bar` and all passed **with the call
deleted from both call sites**. They proved the helper worked, not that it was reached — the
exact failure `[[mutation-test-the-entry-point-not-just-the-layer-below]]` warns about. Fixed
by extracting `closed_candles_since` / `closed_bidask_window` (the whole post-fetch transform)
and testing those, so unwiring either path goes red.

Mutations run, all caught: mid drop removed, bid/ask drop removed, `<=` → `<`, sort removed.
One survived **and should**: swapping the drop and the watermark trim. Both are pure filters
so they commute — the doc comment claiming the order was load-bearing was wrong, and was
corrected rather than propped up with a test that cannot fail.

## Notes

- **No fixture re-bless.** Replay pulls through candle-cache, which already enforced
  `start + bar <= as_of`. Replay was right all along; this brings live up to it.
- **Old timelines keep their forming-bar stamps.** The journal will still show a one-bar Δ for
  TN plans that ran before this fix — a true record of what live did, not a display bug.
