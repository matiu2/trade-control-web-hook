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
- Per instrument, threshold = **`median × 5`** (5× the instrument's own
  normal spread). Chosen 2026-06-23 after spread-hour data showed the
  blowout is FX-only (FX spikes 10–20× normal; Copper/Gold stay flat), so a
  multiple of normal — not the observed max — is the right shape.
- Instruments without a baked value fall back to the flat `8.0`.

## Steps

- [x] build.rs in the worker crate: read `../spread-sampler-cron/samples/*.yaml`,
      compute per-instrument low/high/median `spread_pips` (skip files with no
      `spread_pips`), emit `OUT_DIR/spread_baseline.rs` as a sorted slice.
      `cargo:rerun-if-changed` on the samples dir. Fail-soft → empty table.
- [x] `spread_blackout.rs`: `elevated_threshold_pips(instrument)` consults
      the generated table (`median × SPREAD_REJECT_MULTIPLE` = 5×), falls
      back to `SPREAD_BLACKOUT_ELEVATED_PIPS`. `baked_baseline()` exposed
      for the reject message.
- [x] Update the reject message to name the baked normal/seen-range vs current.
- [x] Tests: table sorted + self-consistent, threshold == 5×median, unknown
      falls back to 8.0.
- [x] cargo build (wasm target check ✓), test ✓, clippy ✓, fmt ✓.
- [x] README: baked-threshold source + re-bake cadence + FX-only finding.
- [x] Commit + push (worker `main`: e5f5ae3 baked table, fdee846 = 5×),
      parent pointer bumped.
- [x] **Deployed to staging (demo)** — landed via another session's
      main→staging merge + `deploy-staging.sh` (2026-06-24). The 5× spread
      threshold is live on the demo worker; spread files identical on
      main/staging.
- [ ] **Re-bake periodically** as samples accumulate. Copper confirmed FLAT
      (~150, no spread-hour) — TradeNation "fixed spreads" change ~twice a
      day, not a liquidity spike. FX is where the spread-hour action is.
```
