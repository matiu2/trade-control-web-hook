# Spread-hour + DST — current state and the canonical workflow

_Last updated: 2026-07-19. Supersedes ad-hoc notes; read this before touching
anything spread-hour related._

## TL;DR

- **The spread-hour mask is a compile-time baked table**
  (`core/src/spread_baseline_candle.rs`, `include!`-ed into
  `core/src/spread_blackout.rs`). **Nothing regenerates it at runtime.** It is
  refreshed only by *manually* running the `spread-baseline-gen` generator and
  rebuilding.
- **Masks are stored in governing-market LOCAL hours**, not UTC. Each row carries
  a `spread_schedule` FK (e.g. `ny` → `America/New_York`); the gate DST-shifts
  the local mask to UTC at read time via `chrono-tz`. So the mask is
  **DST-invariant** — FX + gold sit at **local hour 17 (5pm New York)** all year,
  and the gate resolves that to 21:00 UTC in summer / 22:00 UTC in winter
  automatically. No seasonal re-bake.
- **The hour-mask generator already IS the "pull hourly, then zoom in" workflow**
  you prefer: it pulls **M1** (minute) candles, buckets each minute's spread by
  its schedule-local hour, and flags an hour on its p75 minute-ratio. On-demand,
  not scheduled.
- **The `spread-sampler-cron` OS crontab is RETIRED (2026-07-19).** It fed only
  the per-instrument spread-*magnitude* reject threshold now (the hour timing
  moved to the zoom-in generator), and it had been broken since 2026-07-15 by
  the `instrument-lookup 0.4.0` bump. Its 4 crontab lines are commented out; the
  baked `spread_baseline.rs` snapshot stays so the reject threshold keeps
  working. See §0 below.
- **The loop that IS running** (`blackout_apply_loop` in
  `worker/src/scheduler.rs`, an internal tokio task — NOT an OS cron) does
  **live protection** using the baked mask (widen open stops, cancel resting
  orders, block new entries during the spread hour). It does **not** generate or
  decide masks. Stopping it would remove the live spread-hour protection — see
  "Can we stop the loop?" below.

## Which "cron" is which — the disambiguation that matters

There are THREE things people call "the spread cron". They are different; only
some are candidates for stopping.

### 0. The spread-SAMPLER OS crontab — RETIRED 2026-07-19

`spread-sampler-cron` (a separate repo, `~/projects/trading-libraries/spread-sampler-cron`)
was a **real OS `crontab -e` job** (hourly all day + every 10 min across the
06:30–08:30 Brisbane spread hour). Each tick read TradeNation's live bid/ask
**spread magnitude** per instrument, appended a sweep to `samples/*.yaml`, and
git-committed+pushed. `core/build.rs` reads those samples at **compile time** and
bakes `spread_baseline.rs` (via `OUT_DIR`).

What it fed (and still feeds, from the frozen snapshot):
- **`baked_baseline` → `elevated_threshold_pips`** — the per-instrument
  spread-MAGNITUDE reject threshold ("is the live spread abnormally wide right
  now", System 1). **This is the ONLY thing still tied to the sampler.**
- The sampler *also* baked a per-hour spread-HOUR *timing* mask
  (`baked_spread_hours`), but that role is **superseded** by the candle-derived
  DST-aware mask (§1 below); only the magnitude threshold remains.

**Retired because:** (a) the spread-hour timing already moved to the on-demand
zoom-in generator (§1), so the sampler was down to one job; (b) the
`instrument-lookup 0.4.0` bump (spread-schedule feature) silently **broke** the
sampler's hourly rebuild on 2026-07-15 — it pins `instrument-lookup ^0.3.0`, the
path-dep now resolves to 0.4.0, so `cargo build` fails and the cron errored every
hour for days (samples frozen at `2026-07-15T00:01Z`). Rather than unbreak a tool
we're retiring, the 4 crontab lines were commented out (with a dated note;
`crontab -e`). The **baked `spread_baseline.rs` stays** (Jul-15 snapshot, 1187
rows), so `elevated_threshold_pips` keeps working from the last-good data — a
frozen threshold table is fine (spreads' *normal* magnitude drifts slowly).

**Follow-up (not yet done):** migrate `elevated_threshold_pips` onto the candle
generator's per-instrument spread data (it already computes per-hour p90
`spread/mid` fractions), then delete `spread-sampler-cron` and the
`core/build.rs` sample-baking entirely → ONE spread pipeline. Until then, the
threshold table is a static snapshot; re-bake it by hand only if a new
instrument needs a calibrated reject threshold (run the sampler once manually
after bumping its `instrument-lookup` dep to 0.4).

### 1. Mask GENERATION (spread-HOUR timing) — already on-demand, no cron

The DST-aware hour mask is produced by `spread-baseline-gen` (in the
`trade-control-spread-mask` worktree / crate). You run it by hand, it writes
`spread_baseline_candle.rs`, you commit + rebuild. **This is the "pull hourly,
zoom in" method** — it already is the workflow, and it is *not* scheduled. See
"The canonical mask-refresh workflow" below.

There is no runtime job that samples spreads and rewrites the hour mask. (An
even older "sampler-into-KV" approach that over-flagged whole overnight blocks —
the "12pm Brisbane rubbish" bug — was retired in favour of the candle-derived
table; and the OS-crontab spread-sampler in §0 above is now retired too.)

### 2. Mask APPLICATION — the live protection loop (`blackout_apply_loop`)

(This is an internal tokio loop inside each `trade-control-worker-*` service, NOT
an OS crontab entry — `crontab -l` / systemd timers won't show it.)

`worker/src/scheduler.rs::blackout_apply_loop` wakes every `upkeep_interval`
(~900s) and, using the **already-baked** mask:

- **System 1** — blocks new entries whose live spread is elevated during the
  spread hour (`apply_if_ny_close_edge`, NY-close-edge-gated).
- **System 2** — pre-emptively **widens open-position stops** ~30 min before a
  flagged hour (`widen_open_stops_for_spread_hours`, per-instrument self-gated on
  the baked mask, runs every tick).
- **System 3** — **cancels resting pending orders** before the spread hour and
  restores them after (via the shared `pending_order_lifecycle`).

A sibling watcher (`blackout_watch.rs`) samples the **live** spread only to
detect when the spike is over (recovery), so it can restore the widened stops.
That live sample is *not* used to decide *when* the spread hour is — that is
purely the baked clock mask.

## What decides "is it a spread hour right now?"

`trade_control_core::spread_blackout::is_spread_hour(instrument, now_utc)`:

1. Look up the instrument's row in the baked table → `(schedule, mask, widen)`.
2. Resolve `schedule` → a `chrono_tz::Tz` (`schedule_tz()` in
   `core/src/spread_blackout.rs`, mirrors the instrument-lookup schedule table).
3. Convert `now_utc` → that tz's local hour, index `mask & (1 << local_hour)`
   (with the 30-min lead look-ahead).
4. **Absent instrument / `none` schedule / unknown tz → fall back to
   `ny_clock::is_ny_close_edge`** (a hand-rolled 5pm-NY DST computation). This
   fallback is *correct by default* for any NY-anchored instrument, so a missing
   row degrades gracefully (it just loses the custom widen fraction).

Both the live worker and the offline replay call this same function — replay ==
live by construction.

## The canonical mask-refresh workflow ("pull hourly, zoom in")

Run this by hand whenever masks need refreshing (new instrument, drift, a
new season's worth of data — though DST no longer forces a re-bake). It lives in
the `trade-control-spread-mask` worktree.

```sh
# Binary lives in the WORKSPACE target dir (not the crate dir):
GEN=~/projects/trading-libraries/trade-control-spread-mask/target/release/generate
cargo build --release --bin generate    # from spread-baseline-gen/

# OANDA is FAST (candle API, no paging). Full run in ~2 min:
OANDA_TOKEN=... "$GEN" --brokers oanda --days 60 --out spread_baseline_oanda.rs

# TradeNation M1 is SLOW and flaky PER-INSTRUMENT (some symbols hang forever
# and stall a batch run). Fetch one symbol at a time with a per-symbol timeout,
# collect the successes:
bash -c '
  IFS="," read -ra SYMS < tn_only.txt        # the FX+gold TN symbols
  for sym in "${SYMS[@]}"; do
    safe=$(echo "$sym" | tr "/ " "__")
    timeout 75 env RUST_LOG=error "$GEN" --brokers tradenation --days 10 \
      --only "$sym" --out "/tmp/tn_rows/$safe.rs" >/dev/null 2>&1 \
      && grep -q "\"tradenation\"" "/tmp/tn_rows/$safe.rs" && echo "OK $sym" \
      || echo "FAIL $sym"
  done
'
# NOTE: the interactive shell is zsh — `read -ra` is bash-only, so wrap
# per-symbol loops in `bash -c '...'` or they silently no-op.

# Splice: OANDA file header + oanda rows + TN row lines + `];`, then copy into
# core/src/spread_baseline_candle.rs, rebuild, run the tests, commit.
```

How it works internally (the "zoom in"):

- Pulls **M1** bars per instrument/broker (not H1 — the minute granularity is
  the zoom).
- Computes each minute's `spread_frac = (ask-bid)/mid`, buckets by the minute's
  **schedule-local hour**, resamples mids hourly for a volatility baseline.
- Flags an hour on its **p75** minute-ratio (bulk-of-hour): a ≤10-min
  end-of-hour ramp can't move the p75, so it drops the old **hour-20
  close-boundary bleed**; a genuine ≥¼-hour spike still lifts it.
- Widen size = the hour's **p90** minute-frac. Gate = med3 + peak-fraction.
- A **cross-check** validates each flagged local hour against the schedule
  (e.g. `ny` → expect ~17:00); prints mismatches (misassigned schedule).

Why minute-level, not the H1 the cron used to imply: an H1 candle is sampled at
the bar **close**, so a spike starting in the last minutes of hour N
contaminates hour N's bucket and flags a calm hour. The minute path is
bleed-resistant. This is exactly the "pull hourly, then zoom in to confirm" idea
the user described — the generator does the zoom for you.

## DST — how it's handled (no hardcoded dates)

- Each instrument's spread hour is anchored to its **governing market's local
  wall-clock**. Mapping lives in `instrument-lookup` as a relational
  `[[spread_schedule]]` table + a `spread_schedule` FK on every asset
  (`ny`/`london`/`frankfurt`/`zurich`/`sydney`/`johannesburg`/`hongkong`/
  `singapore`/`tokyo`/`none`).
- Conversion UTC↔local is done with **`chrono-tz`** (the IANA database) — never
  hardcode DST transition dates. FX + gold + US indices → `America/New_York`;
  European indices → their exchange tz; ASX → `Australia/Sydney` (inverted
  southern DST); HK/China-A50/Nikkei → fixed-UTC zones (no DST).
- Empirically confirmed: the FX/gold spike is the **5pm New York rollover**, and
  it shifts 21:00↔22:00 UTC exactly at the US DST dates. London does NOT drive
  FX/gold (its session boundary is hours earlier); it governs only European
  index rows.

## Can we stop the loop / cron?

**The spread-SAMPLER OS crontab — already stopped (retired 2026-07-19, §0).**
The frozen `spread_baseline.rs` keeps `elevated_threshold_pips` alive.

**The hour-mask-generation "cron" — there was never one to stop.** Masks are
refreshed on demand by the zoom-in generator above. If the mental model was "a
job keeps regenerating the hour mask on a schedule," that job does not exist.

**The live-protection cron (`blackout_apply_loop`) — stopping it is a real
trade-off, not a cleanup.** It is what actually protects live positions during
the spike:

- Stop it and you lose System 1/2/3 at run time: open stops won't pre-widen
  before the 5pm-NY blowout, resting orders won't be pulled out of the spike,
  and elevated-spread entries won't be blocked. On a thin cross during the
  rollover that is a real −1R-or-worse exposure (this whole line of work started
  from a GBP/AUD stop-out in exactly that window).
- It is cheap when idle: most ticks self-gate to a no-op (not the close edge /
  no flagged instrument), costing only the clock check.

So: **do not stop the live-protection cron** unless you are deliberately
disabling spread-hour protection for live trading. If the goal is just "don't
run a mask *generator* on a schedule," that goal is already met — the generator
is manual/on-demand.

If you want to reduce its footprint rather than stop it, the levers are: raise
`upkeep_interval` (fewer wakes), or gate System 2's every-tick widen more
tightly. But the current cost is already ~nil when idle.

## Key files

- `core/src/spread_blackout.rs` — the gate: `is_spread_hour`,
  `mask_active_with_lead`, `spread_hour_widen_for/instant`, `schedule_tz`,
  `spread_block_ttl_seconds`, `spread_block_window`. Reads the baked table.
- `core/src/spread_baseline_candle.rs` — the **baked mask table** (6-tuple:
  `broker, symbol, schedule, reviewed, mask_local, widen[24]`). `@generated`.
- `core/src/ny_clock.rs` — hand-rolled 5pm-NY DST fallback (`is_ny_close_edge`).
- `core/build.rs` — compile-time bake of `spread_baseline.rs` (the reject-
  threshold table) from `spread-sampler-cron/samples/`. Fail-soft: missing
  samples → empty table → flat fallback.
- `spread-sampler-cron/` (separate repo) — the RETIRED OS-crontab spread-
  magnitude sampler; `scripts/sample.sh` + `samples/*.yaml`. Crontab lines
  commented out 2026-07-19. Pins `instrument-lookup ^0.3.0` (broken vs 0.4.0).
- `worker/src/scheduler.rs` — `blackout_apply_loop` (live application cron),
  `sweep_loop`, the market-hours refresh.
- `trade-control-cron/src/blackout_apply.rs` — System 1/2 application;
  `blackout_watch.rs` — System 3 lifecycle + live-spread recovery sampling.
- `spread-baseline-gen/` (in `trade-control-spread-mask`) — the on-demand
  generator (`compute.rs::profile_from_minutes`, `fetch.rs`, `bin/generate.rs`).
- `instrument-lookup` `src/schedule.rs` + `[[spread_schedule]]` in
  `catalog.toml` — the relational schedule table + FK.

## Related memories

`[[spread_mask_dst_local_hour_regen]]`, `[[spread_hour_dst_per_market_mapping]]`,
`[[spread_hour_tracks_us_dst_confirmed]]`, `[[spread_hour_rubbish_candle_suppression]]`,
`[[gbpaud_spread_hour_minute_truth]]`, `[[spread_baseline_gen_stage1]]`.
