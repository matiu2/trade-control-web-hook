# TODO — replay fill simulator: bid/ask (half-spread) instead of mid

## Where this belongs (important — settled 2026-06-22)
The replay and the live worker share the **same** decision engine
(`trade_control_engine::evaluate_plan`, both by path dep) — so what *fires* is
identical. They diverge only at the **fill**:
- **Worker:** engine fires → `run_enter` places a real broker order with
  mid-resolved SL/TP → the **real broker fills at real bid/ask**.
- **Replay:** engine fires → `simulate_fill` fakes the fill against MID candles,
  exact-level (no broker off-wasm).

So the live engine triggers on MID (entry/SL/TP are mid-resolved) and the
**broker** applies bid/ask. The fix therefore belongs in the **replay
simulator** (to approximate what the broker does to the worker's order), NOT in
the engine. A short's SL is sent at a mid level but the broker closes it when
the **ask** reaches it — a half-spread earlier. The simulator's ±1-pip
half-spread reproduces exactly that, so it stays faithful to worker+broker.
Do NOT push bid/ask into `evaluate_plan` — that would make the worker trigger
differently than it does today and is a live-behaviour change, not a test fix.

## Problem
`engine::simulator::simulate_fill` is mid-only + exact-level: `candle_crosses`
tests `c.l <= level <= c.h` on MID candles. Real fills happen on the correct
side of the book:

- You **buy at the ask**, **sell at the bid** (ask > bid; spread = ask − bid).
- **Short** (sell to open, buy to close):
  - entry (sell-stop): fills when the **bid** drops to the entry level.
  - SL/TP (buy to close): fills when the **ask** reaches the level.
- **Long** (buy to open, sell to close): mirror — entry on **ask**, exits on **bid**.

Net for a short: SL is effectively reached a half-spread earlier, entry/TP a
half-spread harder. Mid-only replay understates stop-outs.

## Model (mid candles + fixed half-spread)
half_spread `h` = `half_spread_pips * pip_size`; ask = mid + h, bid = mid − h.
Shift the level by `h` before the mid `candle_crosses` test:

| event   | short (test mid range against) | long (test against) |
|---------|--------------------------------|---------------------|
| entry   | `entry + h`  (bid side)         | `entry − h` (ask)   |
| SL      | `SL − h`     (ask side)         | `SL + h`    (bid)   |
| TP      | `TP − h`     (ask side)         | `TP + h`    (bid)   |

Default half_spread = **1 pip** (a 2-pip spread), `--half-spread-pips` to tune.

## Plan
- [x] `simulate_fill` gains a `half_spread: f64` (price units) param.
- [x] Entry-fill test: shift trigger by ±h per direction (bid for short entry,
      ask for long entry).
- [x] Exit test: shift SL and TP by ±h per direction (ask for short exits, bid
      for long exits).
- [x] Record the placed level as the fill price (resting order price); only the *trigger* shifts by half-spread. Originally fill/exit price in SimOutcome (what the broker
      actually gave), not the mid level.
- [x] `--half-spread-pips` flag on replay-candles (default 1.0), thread through
      report -> simulate_fill as `half_spread_pips * plan.pip_size`.
- [x] Tests: a short that survives at mid but stops out once the ask-side SL is
      a pip closer; a long mirror; h=0 reproduces the old exact-level behaviour.
- [x] clippy + fmt. All green (engine 52, cli 247+23+13).

## Verify
NZD/CHF 2026-06-19 fixed plan, arm 13:00: entered 0.46322, "still open" at mid.
Re-check it doesn't stop out (or does) once SL is ask-side + 1 pip.
