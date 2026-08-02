# BUG: every TradeNation row's spread forecast is zero

**Status:** root-caused, fix in progress (re-bake of the 35 TN rows).
**Found:** 2026-08-02, while investigating "the baked forecast looks ~60% too
high for ordinary hours".

## Symptom

The per-hour spread forecast baked for OANDA `AUD_NZD` reads ~2.5p at every
ordinary hour. Measuring the AUD/NZD replay fixture's 529 H1 bars gave p90 =
**1.8p**. At the SL floor's 10× multiple that gap is ~7 pips added to every
stop, so the forecast looked ~60% too high and the generator was the suspect.

## The generator is correct

Re-measured OANDA `AUD_NZD` M1 candles over the generator's own 90-day window
(`spread-baseline-gen/examples/forecast_vs_reality.rs`), computing both the
generator's population (all minutes) and an H1-close reconstruction (`:59`
minutes only) from the same data:

```
hour    n      p50    p75    p90    p99 │  n_cl  cl_p50  cl_p90
  12  3900     2.50   2.60   2.80   3.00 │    65    2.50    2.70
  17  3379    12.50  17.30  23.80  25.00 │    62   11.85   17.70
```

All-minutes p90 = 2.80p, close-only p90 = 2.80p. The two populations agree, so
the "all minutes vs H1 closes" hypothesis is dead: the baked 2.5p is a correct —
if slightly conservative — p90 of fresh OANDA data. Statistic, bucketing and
percentile rule all check out.

## The actual bug: wrong broker's row, and the right one is empty

The fixture's `meta.json` says `"source": "tradenation"`. The 1.8p came from
**TradeNation** candles; the 2.5p forecast is the **OANDA** row. Under the
generator's locked per-broker principle those are different instruments —
OANDA `AUD_NZD` and TradeNation `AUD/NZD` each get their own row, and a TN plan
keys on `"AUD/NZD"`.

That row exists. Its forecast is **all zeros** — and so is every other
TradeNation row:

| broker | rows | forecast populated | all-zero | of which `reviewed = true` |
|---|---|---|---|---|
| oanda | 125 | 120 | 5 | 0 |
| tradenation | 35 | **0** | **35** | **35** |

32 of the 35 carry a populated *widen* column while their forecast is empty —
a self-contradiction, since `apply_gates` copies the widen out of the very
`hour_p90` array the forecast is rendered from. A row cannot have computed one
without the other.

Not a hidden defect: commit `b13f34e`, which introduced the column, says so —
*"The 35 TradeNation rows are carried forward with a zero forecast
(reactive-only until re-baked)."* It was a deliberate, documented deferral. But
the effect is a **silent degrade**, which this repo's rules forbid: for every TN
instrument the forward-looking half of the SL floor vanished and nothing said
so. `spread_forecast_frac` returns `(0.0, 0.0)`, the forecast terms drop out of
the `max`, and the floor quietly falls back to the measured bar alone.

## Why the guard test missed it

`forecast_is_populated_at_unflagged_hours` exists precisely to catch this
("if this ever reads 0.0 at an ordinary hour, the column has been re-gated").
It asserts on `EUR_USD` — an **OANDA** symbol — so all 35 TN rows sailed past a
green test.

Replaced with a whole-table invariant,
`every_reviewed_row_with_a_widen_also_has_a_forecast`: no row may be
`reviewed = true` with a populated widen and an all-zero forecast. It needs no
network — the contradiction is internal to a row. Confirmed red before the fix,
naming all 32 instruments.

**The lesson worth keeping:** a guard that spot-checks one hand-picked symbol
does not guard a *table*. Where an invariant is meant to hold for every row,
assert over every row.

## The correct numbers

Re-baking TN `AUD/NZD` (90d, same generator, batches of 8) populates 24/24
hours:

```
ordinary hours   1.37 – 1.64p     (OANDA row said 2.49 – 2.60p)
local hour 17   16.38p            (OANDA row said 21.51p)
```

1.37–1.64p matches the fixture's measured 1.5p/1.8p. TradeNation quotes AUD/NZD
**tighter** than OANDA in ordinary hours, so applying the OANDA row inflated the
floor by ~1p — about 10 pips of stop at the 10× multiple.

That is not a one-instrument quirk. Comparing the two brokers' own measured
median-spread columns (already in the table, so no fetch needed):

| pair | OANDA median | TN median | TN ÷ OANDA |
|---|---|---|---|
| EUR/USD | 1.60p | 0.50p | 0.31× |
| GBP/USD | 1.90p | 0.80p | 0.42× |
| AUD/NZD | 2.60p | 1.50p | 0.58× |
| GBP/AUD | 5.20p | 1.60p | 0.31× |
| NZD/USD | 1.50p | 1.00p | 0.67× |

TN is tighter on **every** pair checked (0.31–0.67×), so substituting the OANDA
row systematically over-inflates a TN stop floor. AUD/NZD at 0.58× is one of the
*milder* cases.

This does **not** contradict `EXPERIMENT-rubbish-candle.md`'s "TradeNation costs
2.5× more" — the two measure different things:

- That experiment measures the **spike**: how far a broker's spread blows out at
  the flagged hour *relative to its own other hours* (OANDA 4.45×, TN 11×).
- This measures the **baseline**: the absolute ordinary-hour spread level.

TN is tighter most of the time and blows out harder at the NY close. Both facts
live in the same row here — TN AUD/NZD reads 1.37p at ordinary hours and 16.38p
at local 17 (a ~12× spike), against OANDA's 2.49p → 21.51p (~8.6×). A forecast
that is per-broker AND per-hour captures both; a single cross-broker fudge factor
would capture neither. Which is exactly why the per-broker principle is locked,
and why substituting one broker's row for another's is never safe in either
direction.

## Blast radius

- **Shipped v123 `sl_target`** reads this forecast for its max, so every TN
  instrument has been sizing stops reactively (last bar only) since v123.
  Under-forecasting is the *unsafe* direction for a stop-loss floor, but here
  the substituted OANDA row was too WIDE, so the practical effect on AUD/NZD was
  an over-wide floor, not an under-wide one.
- **The corpus A/B** for the experimental entry floor is only partly affected:
  11 of 39 fixtures are TN, 8 of them the same AUD/NZD trade. The other 28 are
  OANDA and were measured against a correct forecast. The AUD/NZD result
  specifically needs re-running once the bake lands.
- `UK 100` and `Coffee` (the other two TN fixtures) are not in the table at all,
  so they degrade to measured-only via the documented path — unaffected.

## Follow-up

- The generator **hangs on a large fan-out** (no error, no timeout, 0 fetches in
  3h) while 8 at a time completes reliably — noted in `b13f34e` and still
  unfixed. A missing fetch timeout is itself a "no silent degrade" violation.
  Re-bakes must stay batched until it is fixed.
