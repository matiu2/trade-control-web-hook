# Track D — make candle-cache compile to wasm32 + add a KV-backed storage backend

You are working in the **candle-cache crate**:
`/home/matiu/projects/trading-libraries/candle-cache`
(a sibling of trade-control-web-hook, NOT inside it). It is its own git repo /
submodule — commit + push there, then the parent pointer is advanced separately.

**Create a git worktree first** (parent CLAUDE.md rule — all work in worktrees).
Place it as a SIBLING of candle-cache so `../` path-deps still resolve:
`git worktree add ../candle-cache-wasm-kv -b feat/wasm-kv-backend` from inside
candle-cache.

## The goal (two-step; you do step 1 = the gating spike + impl)

The trade-control-web-hook Cloudflare **worker** (wasm32, `cdylib`) currently
pulls broker candles **live every cron tick** and does NOT cache them. We want
it to eventually use candle-cache instead, so the *same* pull-and-convert code
runs in both the worker (live) and the offline replay CLI — swapping only the
**storage backend** (disk/LRU off-wasm, Cloudflare **KV** on-wasm).

**Step 1 (THIS task): make candle-cache usable from wasm32 with a KV backend.**
Step 2 (rewiring the worker to call candle-cache) is a SEPARATE later task in
the worker repo — do NOT touch the worker here.

## Phase A — feasibility spike FIRST (cheap, report before building)

Before writing the KV backend, answer with evidence whether wasm32 is even
viable. Do NOT assume it works.

1. `cd candle-cache && cargo build --target wasm32-unknown-unknown` — capture
   what breaks. Likely offenders: `tokio` fs / `rt-multi-thread`, `std::fs`,
   disk-path / eviction code, `std::time::Instant`/`SystemTime`, background
   tasks (`tokio::task::spawn`), any `mmap`/sled/rocks dep.
2. Inventory: which modules are **storage/disk-bound** (need to be feature-gated
   off on wasm) vs **pure pull/convert/aggregation logic** (must compile to
   wasm). Look at `src/storage/mod.rs` — there's already a `StorageBackend`-style
   trait with `get_bid_ask`/`put_bid_ask`/range methods and an in-memory impl
   (`src/storage/memory.rs`). That trait is the seam: if `client.rs`,
   `aggregation.rs`, `cache_key.rs`, `request_optimizer.rs` are generic over it
   and don't themselves touch fs/tokio-fs, the path is open.
3. Check the **data source** side: the worker would feed candle-cache a broker
   data source. Confirm `DataSource` / `BidAskDataSource` traits don't force a
   native-only HTTP client (the worker uses its own broker via `reqwest`-free /
   `worker`-fetch). The KV backend is storage; the data source is separate —
   keep them independent.

**Report the spike result.** If wasm is blocked by something structural
(e.g. tokio fs is load-bearing in non-storage code), STOP and report — don't
force it. We'll rethink. If it's just storage/disk modules behind the trait,
proceed to Phase B.

## Phase B — implement (only if the spike is green)

1. **Feature-gate the disk backend.** Put the disk/LRU/eviction storage impl and
   any `tokio` fs / `std::fs` use behind a `default`-on feature (e.g.
   `disk-storage`). The crate must compile to wasm32 with `--no-default-features`
   plus a `wasm`/`kv-storage` feature.
2. **Add a KV `StorageBackend` impl** behind a `kv-storage` feature. It targets
   Cloudflare KV via the `worker` crate's KV API (`worker::kv::KvStore`). Key
   format: reuse the existing `cache_key` functions (`bid_ask_cache_key`,
   `mid_range_sentinel_key`, etc.) — do NOT invent a new key scheme. Values:
   serde_json of the existing `CacheEntry` / `BidAskCacheEntry`. Honour the same
   trait contract the memory + disk backends do (the `memory.rs` impl is your
   reference for behaviour; mirror its semantics, just persisting to KV).
   - KV is eventually-consistent and has no atomic range scan — implement the
     range/sentinel methods using the sentinel keys already in `cache_key.rs`
     (that's why they exist). Document any consistency caveat in the impl.
3. **Keep the public `CacheClient` API identical** across backends — the caller
   picks a backend at construction, nothing else changes. `get_candles_range`
   and `get_candles_range_bid_ask` must work on both.
4. Tests: the in-memory backend already has tests; add KV-backend tests that run
   off-wasm against a **mock KV** (or the memory backend standing in) so they're
   runnable in normal `cargo test`. Don't require a live CF account to test.

## Rules

- Worktree, sibling placement (path-deps). candle-cache only — do NOT touch the
  worker repo or trade-control-web-hook.
- Spike report BEFORE building. If wasm is structurally blocked, stop + report.
- `cargo build --target wasm32-unknown-unknown --no-default-features --features kv-storage`
  must succeed by the end (that's the acceptance test). Native
  `cargo test` + `cargo clippy` + `cargo fmt` green too.
- 2024 edition, no mod.rs, no unwrap/expect outside tests, color_eyre + tracing.
- Commit + push in the candle-cache repo when green; report the branch + the
  spike findings either way.
