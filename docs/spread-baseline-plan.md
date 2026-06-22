# Spread-baseline calibration plan (replace the flat 8.0-pip blackout threshold)

Status: **PLANNED, not started.** Written 2026-06-19. Survives the context
compact — this file is the source of truth for the design, not the chat
summary.

## Why

The spread-blackout entry gate (`src/lib.rs` ~`run_enter`, message
`entry blocked: spread blackout`, HTTP 423) rejects an entry when **both**:

1. the NY-close spread-hour window is open (`spread-blackout:window`, opened by
   the daily NY-close-edge cron — 21:00 UTC EDT / 22:00 UTC EST, ~15 min), and
2. the live spread exceeds a **flat hardcoded** threshold
   `SPREAD_BLACKOUT_ELEVATED_PIPS = 8.0` (in `src/spread_blackout.rs`),
   instrument-agnostic.

**The bug that triggered this work (2026-06-18):** a Copper Short at 22:11 UTC
was rejected. Copper's `pip_size = 0.1` (from instrument-lookup), price ~13,682.
`spread_pips = spread_price / 0.1`, so a normal ~1.5-price-unit spread = 15 pips
> 8 → always blocked during spread-hour. The flat 8p was tuned for thin FX
crosses (~2p normal, ~20p trough) and is meaningless for a commodity/index.

There is **no** stored spread baseline anywhere today — every cron spread sample
(`blackout_apply`, `blackout_watch`) is read live, used once, discarded. The
"KV store of calculated spreads" was an *intention* (the `TODO(open-question)`
in `src/spread_blackout.rs`), never built.

## Decision: gather samples offline, bake per-instrument constants at compile time

Rejected: a worker cron that samples every instrument every 6h and stores the
last 4 in KV — too much per-tick work cycling all instruments, and KV churn.

Chosen: **a local (native-machine) sampler + git-committed YAML + compile-time
extraction into hardcoded per-instrument constants.** No worker-side catalog
lookup, no new KV, deterministic and signed-build-friendly.

### Steps (this is the agreed plan)

1. **(done first) Compact the context.**
2. **Local sampler script/cron on this machine.** Every hour, sample the live
   spread for **all** tradable instruments (via the TradeNation client / the
   `tradenation` MCP path or a small Rust bin), append to on-disk **YAML files**
   (one per instrument, or one rolling file — TBD at build time). Record at
   least: timestamp (UTC), instrument, bid, ask, spread (price units), pip_size,
   spread_pips. Friday caveat: a full 24h cycle won't be captured immediately
   (weekend close) — keep sampling across the next full trading week.
3. **Commit the YAML samples to git.** At **compile time** (build.rs), extract
   **three numbers per instrument** from the samples:
   1. **low spread** — the tight spread seen during **London and/or NY open**
      (liquid hours). The "normal" figure.
   2. **high spread** — the wider spread when **NY and London are closed**.
   3. **spread-hour spread** — the very high spread **just after NY close**
      (the spike we must never enter on).
4. **Use the hardcoded numbers in the gate + message.**
   - **Gate:** only reject when `current_spread > high spread` (the
      blackout/spike level), per-instrument — replacing the flat 8p. (Exact
      relationship between "high" and "spread-hour" for the reject threshold to
      be finalised when we have data; intent is: block the spike, allow normal +
      mildly-wide.)
   - **Message:** e.g.
     `copper small spread = 6 pips, large spread = 15 pips, current spread 24 pips; preventing entry/order placement for safety`

## Open questions to resolve when data is in

- One YAML per instrument vs. one rolling file; retention.
- How build.rs picks low/high/spread-hour from the samples (percentiles? fixed
  session-time buckets via UTC minute-of-day, reusing the blackout session
  windows?).
- Exact reject threshold: `> high` vs `> spread-hour` vs `> k × low`.
- Whether to keep the NY-close *window* gate at all, or let per-instrument
  thresholds + live spread fully replace the time-window condition.
- Units: keep everything in **pips** (`spread_price / pip_size`) consistently,
  as the rest of the spread-blackout feature already does.

## Touch points (where the change lands)

- `src/spread_blackout.rs` — `SPREAD_BLACKOUT_ELEVATED_PIPS` flat constant +
  `elevated_threshold_pips(instrument)` (already takes the instrument arg and
  ignores it — wire the per-instrument table in here).
- `src/lib.rs` — the spread-blackout gate in `run_enter` (~line 1590-1605):
  threshold lookup + the reject message body.
- `build.rs` (new logic) — parse committed YAML → generate per-instrument
  constants.
- Local sampler — new, lives on this machine (script or small bin); samples
  source = TradeNation.
