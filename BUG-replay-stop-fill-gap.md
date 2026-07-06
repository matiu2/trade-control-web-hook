# BUG — replay under-reported fills on a gap-through trigger

**Found:** 2026-07-06, replaying AUD/NZD iH&S long (`ihs-aud-nzd-37d31f02`) via
`replay-candles --plan /tmp/trade.json --source oanda`. The report said the entry
`NEVER FILLED (pending order untriggered in window)` even though the chart clearly
traded through the trigger and would have taken profit.

## Symptom

```
08:00  entry #1 placed — LONG stop @ 1.21634
08:00  entry #1 fill: NEVER FILLED (pending order untriggered in window)
11:00  Veto (01-veto-too-high) — no fill simulated
Done: true | fires: 4 | TP: 0 SL: 0 | Net R: +0.00
```

But the OANDA H1 candles after the 08:00 fire bar:

| bar (+10) | ask_open | ask_high | ask_low |
|---|---|---|---|
| 09:00 | 1.21644 | 1.21703 | **1.21641** |
| 10:00 | 1.21680 | 1.21695 | **1.21637** |
| 11:00 | 1.21660 | 1.21898 | **1.21647** |

Price **gapped up** between the 08:00 close and 09:00 open: every post-fire bar
*opened above* the 1.21634 trigger, so no bar's low ever dipped back to it.

## Root cause

`engine/src/simulator.rs` decided a stop/limit filled with a **containment**
predicate — `book_crosses`: `low <= level && level <= hi`. The trigger had to sit
*inside* the bar's range. A bar that gaps or opens already past the trigger (its
whole range on the far side) failed the test, so the order was reported
`NeverFilled` — even though a real broker stop fills the instant price trades
through the trigger.

The same latent bug lived on the **exit** leg: a bar that gapped through the SL or
TP (both extremes past the level) would likewise have been missed.

## Fix

Replaced the direction-agnostic bracket test with a **directional touch**,
`book_reaches` + `Approach::{FromBelow, FromAbove}`:

- **FromBelow** (price rises into the level) ⇒ `high >= level` — long-stop entry,
  long take-profit, short stop-loss.
- **FromAbove** (price falls into the level) ⇒ `low <= level` — short-stop entry,
  short take-profit, long stop-loss.

Applied to all six touch sites: the entry trigger in `find_fill`, and the SL/TP
checks in Phase 2, `breakeven_armed_at`, and `widened_stop_at`.

## Scope — replay-only

The **live worker doesn't share this code** — it places a real broker stop, so the
broker always filled these correctly. The bug only made the **offline replay
under-report fills**, which skewed journaling/backtest R accounting on gappy
entries. No live behaviour changes. (Per the "strategy changes go in both replayer
+ worker" rule: verified the worker delegates fills to the broker, so there is no
worker-side mirror to change.)

## Tests

`engine/src/simulator.rs`: `long_stop_fills_when_price_gaps_up_through_trigger`,
`short_stop_fills_when_price_gaps_down_through_trigger`,
`long_stops_out_when_price_gaps_down_through_sl`. All 39 simulator tests + the 69
replay tests (incl. `all_fixtures_match_expected`) pass; no golden fixture shifted.

## Verification

Re-running the AUD/NZD replay after the fix:

```
09:00  entry #1 FILLED @ 1.21634
11:00  entry #1 SL→break-even (a candle closed past 50%-to-TP)
15:00  entry #1 TOOK PROFIT → 1.21897   (R: +1.09 | +$1094 → $101094)
Net R: +1.09 | TP: 1 SL: 0
```
