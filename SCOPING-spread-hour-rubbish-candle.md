# SCOPING — spread-hour candles are "rubbish": suppress entries, signals, and level crosses

## Origin

Replay of `tv-arm-staging --start '2026-07-08T23:00:00+10:00' --replay
--strategy-v2` on AUD/CHF showed a −2R over the window. Entry #1 (a SHORT stop
resting @ 0.55980, SL 0.56070) **filled and stopped out in the same H1 bar** at
2026-07-08T21:00Z (= 07:00 Brisbane).

That bar is the single most-elevated **learned spread hour** for AUD/CHF
(baked mask bit 21, p90 widen **12.0p** — the biggest of the day). Tick check
of that bar:

- 20:45Z close: bid 0.5603 / ask 0.5604 — spread ~0.9p (normal).
- **21:00Z open: bid 0.55959 / ask 0.56079 — spread ≈ 12p.** The bracket
  (entry 0.55980, SL 0.56070) sits *entirely inside* the blown-out spread.
- bid gapped through the 0.55980 short-stop → fill; ask was already 0.56079
  (above the 0.56070 SL) → instant stop-out. Same instant.

The candle is not a market move — it's a **liquidity-vacuum spread explosion**.
The operator's framing: **treat spread-hour candles as rubbish data** — not a
reflection of the real market — and refuse to originate any action from them.

(A companion analysis showed a 12p pre-emptive *stop widen* on the open
position would have survived — max ask 0.56139 < widened SL 0.56190 — and the
trade then ran toward TP. That is the **already-built widen system for open
positions** and is complementary. This doc is about *not acting on the rubbish
candle in the first place*, which the operator chose as the primary model.)

## Principle

On a **spread-hour bar** (per-instrument baked mask, `is_spread_hour`, incl.
the 30-min lead), the candle's OHLC is untrustworthy. The engine must **not**:

1. **Fill a new entry** (market entry, or a pending stop/limit trigger).
2. **Detect a signal** (golden / confirmed) from that bar.
3. **Trigger a level cross** (break-and-close, retest, invalidation, M/W
   overshoot/cancel).

The engine **must still**:

- **Honour SL/TP exits** on these bars. A real broker stops you regardless — we
  cannot suppress a broker stop. That exposure is handled by the *separate*
  pre-emptive **stop-widen** for **open** positions (already built). Do NOT
  touch exit honouring here.
- Keep pending orders **live** (don't cancel) — see Pending fills below.

## Decisions (locked with operator)

- **Suppress:** new entries + signal detection + level crosses. **NOT**
  stop-outs.
- **Home:** the shared **`engine`** (+ the pure predicate in `core`) so
  **replay and the live worker behave identically**. This is the
  `[[strategy_changes_in_both_replayer_and_worker]]` invariant.
- **Trigger:** the **baked clock** `is_spread_hour(instrument, bar_time)` —
  pure, deterministic, no live quote, no store read. The existing
  **spread-blackout window + poll-until-calm** System-1 entry reject
  (`core/src/dispatch/enter.rs` `get_spread_blackout_window`, `blackout_watch`)
  is **live-only** and stays as a *layer on top* that can extend the block past
  the baked hour when the live spread hasn't recovered. Replay models only the
  deterministic baked-hour core (it has no ticks to poll) — acceptable, because
  the baked hour is the deterministic spine.
- **Lead:** reuse `SPREAD_HOUR_LEAD_MINUTES` (30 min). `is_spread_hour`
  already bakes the lead in via `spread_hour_widen_pips`.
- **Un-sampled instruments:** fall back to `is_ny_close_edge` (the existing
  `is_spread_hour` fallback) — so fresh assets still get the block.
- **Pending fill on a spread-hour bar:** **reject the fill; leave the order
  resting.** Next non-spread-hour bar re-triggers if price still qualifies.
  Matches the stateless "next signal re-fires" pattern of System-1. Do NOT
  cancel the order.

## Where it lands (call sites)

Pure predicate already exists: `core::spread_blackout::is_spread_hour(instrument, now)`.
No new baked data needed. Suppression wires that predicate into the engine's
per-bar evaluation.

> Runtime note: this is the **native local Postgres worker** (`worker/`), not a
> Cloudflare Worker — Cloudflare + wasm are fully retired and `src/lib.rs` is
> deleted. "Window"/state is a Postgres-backed `StateStore` trait method
> (`get/set_spread_blackout_window`), **not** KV. The pure `is_spread_hour`
> predicate touches no store at all.

1. **Signal detection** — engine bar evaluation (`engine/src/evaluate.rs`,
   `BarEvent` / signal-detector path). Skip golden/confirmed marking when
   `is_spread_hour(instrument, bar.time)`. Replay's detector summary + the
   worker's `dispatch_fired` both consume this.
   - Report/verbose: mark such bars visibly (e.g. `⌀ spread-hour (rubbish) —
     detection suppressed`) so a suppressed golden isn't silently missing.

2. **Level crosses** — `engine/src/evaluate.rs::level_crossed` (and the
   trendline/`PriceValueCross` consumers). A cross whose bar is a spread hour
   does not fire. Covers break-and-close, retest, invalidation, M/W triggers.

3. **Entry fill** —
   - **Live worker:** `core/src/dispatch/enter.rs`, alongside the existing
     spread-blackout reject. Add a baked-clock reject **before** the
     `get_spread_blackout_window` (Postgres) path: `if
     is_spread_hour(resolved.instrument, now) { reject: spread-hour }`.
     Stateless, no store write, no `mark_seen` (Skip in `seen_decision`) — must
     not poison the intent id (see CLAUDE.md "Replay protection scope").
   - **Replay:** `engine/src/simulator.rs::find_fill` — a pending Stop/Limit
     that would fill on a spread-hour bar is **skipped** (continue to the next
     bar in the fill scan), not filled. Market entry whose fire/shell bar is a
     spread hour → the worker would have rejected, so the replay reports it
     not-taken.

## Interaction with the existing widen (keep both)

- **Open position + spread hour → widen the stop** (built:
  `blackout_apply::widen_open_stops`, replay `widened_stop_at`). Unchanged.
- **Rubbish candle → don't originate entries/signals/crosses** (this doc).

These are complementary and must not be conflated: the widen protects a
position that is *already on*; the rubbish-candle suppression stops us
*starting* something off a garbage bar. The bug that started this
(fill-into-the-spike) is fixed by the entry suppression here; the widen covers
the case where we were already in before the hour.

## Tests (first)

- `is_spread_hour` true bar: golden signal **not** detected; a break-and-close
  cross on that bar does **not** fire; a pending stop that would fill there is
  **skipped** and can fill on the next clean bar.
- `is_spread_hour` false bar: byte-identical to today (regression guard).
- Un-sampled instrument at NY-close edge: suppressed via fallback.
- 30-min lead: a bar 20 min before an elevated hour's top is suppressed.
- SL/TP on a spread-hour bar **still** exits (exit honouring untouched).
- Replay ↔ worker parity: the same AUD/CHF 2026-07-08 window no longer fills
  entry #1 on the 21:00Z bar (reports it deferred/not-taken), and the −R from
  that fill is gone.

## Non-goals / follow-ups

- Not building a replay poll-until-calm (no ticks offline) — baked hour only.
- `tv-arm` per-trade override flag to disable rubbish-candle suppression:
  not now.
- Widening the fallback beyond NY-close for un-sampled assets: not now.
