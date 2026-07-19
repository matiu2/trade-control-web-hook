# TN H4 (+ M5) — adapter-side aggregation plan

**Status:** NOT built. The path is clear and unblocked (no external dependency).

## Corrected understanding (2026-07-19)
Earlier plan assumed a coming `tradenation-api` H4 endpoint + a flag flip. **Wrong.**
Confirmed by the tradenation-api maintainer + code:
- TN's raw OHLCV endpoint (`tradenation_api::ohlcv::get_candles_range`) serves
  **only** minute/quarter(15m)/hour/day, hardcoded in `granularity_to_path`.
  **There is no coming change on that side** — the `broker-tradenation` crate is
  orders/login/sizing only, no candle handling.
- **`candle-cache` already fully supports H4** — `CandleAggregator` with verified
  **00/04/08/12/16/20 UTC** bucket alignment (`align_timestamp_to_granularity`,
  `aligned = (epoch/target)*target`; epoch is midnight UTC so H4 lands on those
  boundaries — matches OANDA + TradingView). All three PriceTypes (Mid/Bid/Ask)
  go through the same `aggregate_candles`. M5 same mechanism on 00/05/10/… min.
- The live TN adapter **does not use candle-cache** (it calls the raw API直接).

So the fix is **adapter-side aggregation**, entirely in THIS repo. `TN_SERVES_H4`
/`TN_SERVES_M5` stay `false` **forever** (they mean "native endpoint exists",
which it never will) — H4/M5 are handled by a separate aggregation branch.

## Implementation (approach: call CandleAggregator directly)
Chosen over routing through `CacheClient` — keeps the caching/storage/eviction
layer + its config out of the live worker; reuses only the verified reducer.

1. **Dep:** add `candle-cache = { path = "../../candle-cache" }` to
   `broker-tradenation-adapter/Cargo.toml` (sibling-of-repo path; worktrees must
   be siblings for the `../` to resolve — see `[[worktree_path_dep_symlinks]]`).
2. **`get_candles` (mid):** when `granularity` is H4/M5, fetch the **native base**
   (H1 for H4, M1 for M5) via the existing `get_candles_range` path with the
   count-back scaled to the base TF (`candle_count_for_window` at the base
   granularity → covers `(since, now]`, ×4 / ×12 bars). Then
   `CandleAggregator::new(true).aggregate_candles(base, H1, H4)` → map
   `CandleData` → `core::broker::Candle`. `filter_new_candles(.., since)` as now.
3. **`get_bidask_candles`:** same, but the base fetch is 3× (Mid/Bid/Ask), each
   aggregated independently, then zipped by timestamp (as the native path does).
4. **Type bridge:** aggregator takes `impl candle_model::Candle` and returns
   `candle_model::CandleData`; the raw TN fetch → a candle_model type → aggregate
   → `core::broker::Candle` / `BidAskCandle`. Keep the bridge in one helper.
5. **Base-fetch alignment guard:** the base H1 window must start on/before the
   first H4 bucket boundary covering `since`, or the leading H4 bar is built from
   a partial H1 group. Over-fetch a few base bars of slack (the aggregator drops
   incomplete leading groups — verify it does; if not, trim the first partial bar).

## MUST-VERIFY (parity, not just "it returns candles")
- **Bucket alignment live:** fetch an H4 window through the adapter, assert every
  bar's `secs % (4*3600) == 0` and timestamps are 00/04/08/12/16/20 UTC.
- **replay == live:** run the same H4 plan through `replay-candles` and the live
  adapter (broker-check or a scratch bin) and diff the candle series — the whole
  point is live now matches what replay already does.
- **Partial trailing bar:** the current (still-forming) H4 bar must NOT be emitted
  as closed — the engine's watermark logic assumes closed bars only.

## Post-build
- Re-arm the TN H4 setups deleted 2026-07-19 (see
  `NOTE-bricked-tn-h4-plans-to-rearm.md`): ihs-aud-cad (long), hs-eur-jpy (short),
  hs-gbp-jpy (short), m-usd-jpy (short).
- Consider an arm-time reject of TN H4/M5 in tv-arm as a footgun-guard UNTIL this
  lands (so no new dead plan is created in the meantime).
