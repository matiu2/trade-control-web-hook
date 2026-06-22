# TODO — bake per-instrument spread thresholds into the worker

Goal: replace the flat `SPREAD_BLACKOUT_ELEVATED_PIPS = 8.0` with a
per-instrument threshold baked at compile time from the
`spread-sampler-cron` submodule's committed YAML samples. Ship today on
demo; re-bake tomorrow with more live data.

## Design (settled)

- Gate (`src/lib.rs` ~1590) compares `spread_pips = quote.spread()/pip_size`
  against `elevated_threshold_pips(&resolved.instrument)`.
- `resolved.instrument` is the **broker-canonical (TradeNation MarketName)**
  symbol — same key as the samples. Threshold unit is **pips**.
- Per instrument, bake the **observed max `spread_pips`** as the threshold
  (reject only when current exceeds the instrument's own observed ceiling).
- Instruments without a baked value fall back to the flat `8.0`.

## Steps

- [ ] build.rs in the worker crate: read `../spread-sampler-cron/samples/*.yaml`,
      compute per-instrument max `spread_pips` (skip files with no
      `spread_pips`), emit `OUT_DIR/spread_baseline.rs` as a `match` fn.
      `cargo:rerun-if-changed` on the samples dir.
- [ ] `spread_blackout.rs`: `elevated_threshold_pips(instrument)` consults
      the generated table, falls back to `SPREAD_BLACKOUT_ELEVATED_PIPS`.
- [ ] Update the reject message to name the baked normal/high vs current.
- [ ] Tests: generated-table lookup hits a known instrument (Copper,
      EUR/USD), misses fall back to 8.0.
- [ ] cargo build (wasm target check), test, clippy, fmt.
- [ ] README: note the baked-threshold source + the re-bake cadence.
- [ ] Commit + push (worker repo), bump parent pointer.
- [ ] Deploy to **staging** (demo) — the user wants to go live on demo ASAP.
```
