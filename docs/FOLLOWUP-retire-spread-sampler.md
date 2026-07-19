# Follow-up: fully retire spread-sampler-cron

**Status:** OS crontab STOPPED 2026-07-19 (4 lines commented out). This is the
plan to delete the tool entirely. Not urgent — the frozen snapshot works.

## Current state

`spread-sampler-cron` is retired at the schedule level but not deleted. Its only
remaining consumer is `elevated_threshold_pips` (System 1's per-instrument
spread-MAGNITUDE reject threshold), which reads `baked_baseline` from
`core/src/…/spread_baseline.rs`, baked at compile time by `core/build.rs` from
`../../spread-sampler-cron/samples/*.yaml`. That table is now a **static
snapshot** frozen at `2026-07-15T00:01Z` (1187 rows). Spreads' *normal*
magnitude drifts slowly, so a static snapshot is acceptable indefinitely.

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
