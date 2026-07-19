# Follow-up: fully retire spread-sampler-cron

**Status:** OS crontab STOPPED 2026-07-19. **Code consumption DELETED 2026-07-19
(v91, commit 78bd280).** `spread-sampler-cron` no longer feeds anything — the
only remaining step is deleting the repo itself (optional cleanup).

## Current state (post-v91)

`elevated_threshold_pips` (System 1's per-instrument spread-MAGNITUDE reject
threshold) now reads `baked_baseline` from the **candle table**
(`SPREAD_BASELINE_CANDLE`, columns `baseline_(median,low,high)_pips`), produced
by the on-demand zoom-in generator `spread-baseline-gen`. Gate math unchanged
(`median × SPREAD_REJECT_MULTIPLE`, flat fallback). Real pips regenerated
(OANDA+TN, 160 rows) — e.g. TN EUR/USD 0.5p, OANDA gold 64p.

**Deleted in v91:** `core/build.rs` (the sampler YAML bake), the
`mod baseline { include!(OUT_DIR/spread_baseline.rs) }`, `baked_spread_hours`,
the dead `spread_hour_widen_pips`, and the `[build-dependencies]`. Net −201
lines. There is now **ONE spread pipeline** (the candle generator).

`spread-sampler-cron` (the sibling repo) and its commented-out crontab lines are
now fully DEAD — nothing reads their output. They can be deleted at leisure; left
in place only as historical reference.

## ~~Migration~~ — DONE in v91

(The plan below is complete. Retained for history.)

## Why it was stopped, not just fixed

The spread-HOUR *timing* already migrated to the on-demand candle generator
(`spread-baseline-gen` → `spread_baseline_candle.rs`, DST-aware local-hour
masks). The sampler was down to just the magnitude threshold. It had also been
broken since 2026-07-15: it pins `instrument-lookup ^0.3.0`, but the
spread-schedule feature bumped `instrument-lookup` to `0.4.0`, so its hourly
`cargo build` failed. We chose to retire rather than unbreak a redundant tool.

## The migration (to delete it entirely)

The candle generator already computes, per instrument, the per-hour p90
`spread/mid` fraction. `elevated_threshold_pips` needs a per-instrument *normal*
spread in **pips** (it returns `median × SPREAD_REJECT_MULTIPLE`, fallback flat
`SPREAD_BLACKOUT_ELEVATED_PIPS`).

Plan:
1. Extend `spread-baseline-gen` to emit, alongside the hour mask + widen, a
   per-instrument **baseline median spread in pips** (it has the minute spreads
   already; take a whole-window median, convert frac→pips with the instrument's
   pip size from `instrument-lookup`).
2. Add that column to the candle table tuple (or a sibling table) and point
   `baked_baseline` / `elevated_threshold_pips` at it.
3. Delete `core/build.rs`'s sample-baking, the `spread_baseline.rs` include, and
   the `spread-sampler-cron` repo + its (already-commented) crontab lines.
4. Result: ONE spread pipeline (the zoom-in candle generator), no OS cron.

## If you need to re-bake the threshold before the migration

Only if a NEW instrument needs a calibrated reject threshold (existing ones use
the frozen snapshot; unknown ones fall back to the flat constant, which is safe
for thin FX and over-permissive for wide instruments):

```sh
cd ~/projects/trading-libraries/spread-sampler-cron
# unbreak the build first:
#   Cargo.toml: instrument-lookup = "0.3" → "0.4"
cargo build --release
./target/release/spread-sampler-cron --out-dir samples   # one manual sweep
git add samples && git commit -m "manual sweep" && git push
# then rebuild core so build.rs re-bakes spread_baseline.rs
```

Do NOT re-enable the crontab — a manual sweep when needed is enough.
