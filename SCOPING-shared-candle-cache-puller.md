# SCOPING — route the live worker's candle fetch through `candle-cache` (shared puller)

**Status:** SCOPED, not implemented. The low-risk half (shared ATR-aware
detector-window depth) shipped separately on branch
`feat/shared-candle-cache-puller`; this doc covers the larger re-plumb.

## Why

Live and replay pull candles through **different code**:
- **Replay** → `candle_cache::CacheClient<D: DataSource>`
  (`cli/src/bin/replay_candles/candles.rs::pull`), disk-cached, bid/ask.
- **Live worker** → broker adapters directly (`broker-oanda/src/candles.rs`,
  `broker-tradenation-adapter/src/lib.rs`) with raw per-tick HTTP GETs, **no
  cache**.

Because the two fetch paths differ, they can return **different candle sets for
the same bar** — the source of the feed-close / bar-alignment divergences seen in
the 2026-07 reconciliation (`hs-eur-zar`, `hs-nzd-usd`). Routing **both** through
one `candle-cache` path makes the replay a faithful predictor of live — the whole
point of replay. It also gives the live hot path a cache (today it re-hits the
broker every 5 s per plan).

Historical context: the live worker had its own broker-direct puller because on
Cloudflare/WASM the disk-backed `candle-cache` couldn't run in the worker. **That
constraint is dead** — we're 100 % local, WASM is fully retired
(`[[cloudflare_wasm_fully_retired_from_tree]]`), and `candle-cache` is already a
dependency here (the replay uses it). So the shared puller is now possible.

## The shared-depth half — DONE (this branch)

The ATR-starvation golden bug (`[[live_detector_window_atr_starved]]`) is fixed
independently of this migration: `core::signals::detector_lookback_bars(cfg, g) =
max(min_lookback_bars, atr_length_for(g)) + slack`, called by BOTH the live
`pine_lookback_since` (`trade-control-cron/src/engine.rs`) and the replay warmup
floor (`cli/src/bin/replay_candles.rs`). That's the *window-depth* seam. This doc
is the *fetch-path* seam — a separate, larger change.

## Migration surface — 3 live call sites (all currently `Broker::get_candles*`)

| # | Site | Fetches | Notes |
|---|---|---|---|
| A | `trade-control-cron/src/engine.rs:613` `fetch_candles` | MID | central puller — called from main tick (`:152`), `seed_first_tick` (`:596`), `detector_window_for` (`:669`) |
| B | `trade-control-cron/src/breakeven_watch.rs:231` `fetch_candles` | MID | duplicated `BrokerHandle`-match wrapper — migrate alongside A |
| C | `core/src/dispatch/enter.rs:1273` `get_bidask_candles` | BID/ASK | entry SL-spread-floor `mean_spread`; generic `B: Broker`, shared by webhook `run_enter` + engine dispatch |

Watchers that do **not** fetch candles (verified): `sweep.rs`,
`blackout_apply.rs`, `blackout_watch.rs`, `blackout_hours.rs`,
`spread_lifecycle.rs` (all use `get_quote`/`get_current_price` only).

The replay's field-map (`candles.rs::to_engine_candle`) and granularity maps
(`to_oanda` / `to_cm_granularity`) are directly reusable.

## Design

- Build ONE long-lived `CacheClient` **per data source** at worker boot, held on
  `AppState` (like the Postgres pool + secrets). `CacheClient::new` is async,
  opens a storage backend, and (with `disk-storage`) spawns a background eviction
  task — it is **not** meant to be rebuilt per tick. The live worker must reuse it
  across ticks.
- A shared `candle_puller` module (in `core` or a new small crate) exposing the
  same closed-only / strictly-after-`since` / ascending / mid-or-bidask contract
  the `Broker` trait promises today (`core/src/broker.rs:364-408`), implemented
  over `CacheClient`. Both live (A/B/C) and replay call it.
- DataSource construction reuses the worker's existing creds
  (`worker/src/broker_factory.rs::acquire_oanda`/`acquire_tn`): OANDA needs
  `OandaDataSource::new(OandaClient, account_id)`; TN needs a
  `tradenation_api::TradeNationClient` (impls `DataSource`+`BidAskDataSource`),
  a *second* client from the same account for data (separate from the
  order-placing session broker — the replay already does this).

## ⚠️ BLOCKERS — must each be solved before this is safe on the live hot path

1. **Eviction `tokio::spawn` PANICS on the cron's current-thread runtime.**
   The cron runs on a dedicated current-thread runtime + `LocalSet` (because the
   broker SDKs are `!Send`). `candle-cache`'s `disk-storage` background eviction
   uses `tokio::spawn` (`candle-cache/src/client.rs:87`), which **panics** on a
   `new_current_thread` runtime. A panic here freezes the whole scheduler — the
   exact failure class as `[[retest_tolerance_panic_kills_cron_loop]]`.
   **→ Must construct the client with background eviction OFF (or a
   non-spawning storage backend), and verify the whole `CacheClient` is `Send`
   on the chosen backend.** The data-source *types* (`TradeNationClient`,
   `OandaDataSource`) ARE `Send+Sync`, so a `Send` `CacheClient` is legal on the
   `LocalSet`; it's only the eviction spawn that bites.

2. **The still-forming-bar `complete` flag is LOST through `candle_model::CandleData`.**
   Live's `broker-oanda/src/candles.rs:60` drops the forming bar via OANDA's
   authoritative `complete == false`. `candle-cache` returns `CandleData`, which
   has **no `complete` field** — the flag is gone before the puller sees it. A
   time-based "drop the last bar if its open ≥ current bar boundary" approximation
   can differ from the authoritative flag on a freshly-closed boundary or a
   spread-hour rubbish candle, **shifting entry/cross timing by a bar** on the
   live engine. **→ Decide: extend `candle_model::CandleData` to carry
   `complete`, or wrap the `DataSource` to drop the forming bar before caching.**
   This is the correctness blocker that most needs a decision first.

3. **`filter_new_candles` + missing-block filter must be RE-APPLIED.**
   `candle-cache`'s range methods don't reproduce the worker contract: OANDA's
   data source doesn't filter at all; TN does a half-open `[from,to)` retain. The
   puller must still run `filter_new_candles(candles, since)` (strictly-`>since`,
   sorted). The "drop candles missing a MID/bid-ask block" filter
   (`broker-oanda/src/candles.rs:61`) **cannot** be re-applied post-cache (block
   info flattened to `f64`) — verify what `CandleData::from` does with a partial
   `MBA` block.

4. **TN-H4/M5 brick is NOT fixed by this migration.** Aggregation runs ONLY in
   `CacheClient::get_candles(count)` (`client.rs:159`), NOT in the `get_candles_range`
   / `get_candles_range_bid_ask` methods the engine + replay use (the bid/ask
   method's doc literally says "No aggregation"). So routing through the range
   API leaves TN H4/M5 → `BadRange` exactly as broken
   (`[[tn_h4_m5_plans_permanently_bricked]]`). Fixing TN-H4 is a **separate**
   change (add range-aggregation to candle-cache, or reject/aggregate at
   registration). **Decouple it from this migration.**

## Other risks (rank-ordered)

5. **Cache staleness on the trailing bar.** candle-cache serves from disk when it
   thinks coverage is complete; a bar cached while still forming, or a broker
   revision, could be served stale to the live 5 s cron (which today always reads
   live). Needs a "never cache / always-refresh the trailing bar" policy.
6. **Bar-set change → full replay-vs-live parity re-run REQUIRED.** The migration
   changes the exact candle set for both brokers; every bar-sensitive gate
   (break-and-close, retest tolerance, SL-floor mean-spread) is affected. This is
   `[[strategy_changes_in_both_replayer_and_worker]]` in reverse. Re-run the
   week's 32-plan reconciliation and confirm MATCH count doesn't regress.
7. **Error mapping.** Map `CacheError`/storage errors → `CandleError::Transient`
   so a cache-backend hiccup degrades to "skip this tick", never a panic.
8. **`OandaBroker.client` is private** — add a `pub fn client()`/`data_source()`
   accessor or rebuild `OandaClient` from the api_key in `broker_factory.rs`.
9. **Pagination seams** differ (candle-cache splits at 5000; TN adapter chunks at
   1000) — verify no gap/dup at seams on wide back-windows for both brokers.

## Suggested sequencing

1. Solve blocker #2 (`complete` flag) — likely `candle_model::CandleData.complete:
   bool` (`#[serde(default = true)]`), threaded through the OANDA/TN data sources.
2. Build the shared `candle_puller` over `CacheClient` with eviction OFF (#1),
   re-applying `filter_new_candles` (#3), mapping errors to `Transient` (#7).
3. Hold ONE `CacheClient` per source on `AppState`; migrate A, then B, then C.
4. Full parity re-run (#6). Only promote to staging after MATCH count holds.
5. TN-H4 aggregation (#4) and trailing-bar freshness (#5) as follow-ups.

Do NOT attempt 1-4 as a single commit on a live-trading hot path.
