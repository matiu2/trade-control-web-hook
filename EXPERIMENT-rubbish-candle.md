# EXPERIMENT — is a spread-hour candle actually "rubbish"?

**Run:** 2026-07-31 · OANDA H1 · 4 years (2022-07 → 2026-07)
**Sample:** 66 instruments · **71,351 flagged bars** vs **1,564,202 control bars**
**Tool:** `cli/examples/spread_hour_candle_stats.rs`
**Raw:** `/tmp/claude-1000/spread-hour-stats.json`

---

## Result: the folklore is NOT supported

The engine suppresses signals on spread-hour bars at 5 sites in
`engine/src/evaluate.rs`, justified by the inherited claim that the print
"doesn't reflect real price" — internal broker matching or an absent liquidity
provider, so the OHLC is an artefact of who was quoting.

That claim predicts the move **should not stick**. Measured against the same
instrument's other hours:

| metric | median ratio | folklore predicts | verdict |
|---|---|---|---|
| `persist1` — move alive 1 bar later | **0.98** | ≪ 1 | ❌ **no** |
| `persist3` — alive 3 bars later | **1.11** | ≪ 1 | ❌ **no** (more persistent) |
| `retrace1` — given back next bar | 1.24 | ≫ 1 | ~ weak yes |
| `wick_share` | 1.12 | ≫ 1 | ~ weak yes |
| `range_over_spread` | **0.16** | ≪ 1 | ✅ **yes** — but see below |
| `range_over_atr` | **0.76** | (n/a) | bars are **smaller**, not bigger |

Medians, not means: per-instrument spread is wide (`persist1` sd 0.76) and a few
exotics dominate the mean.

### The one metric that "supports" it doesn't survive decomposition

`range_over_spread` collapsing to 0.16 looks like the strongest possible
evidence — until you ask *which side of the ratio moved*. Since
`range_over_atr = 0.76` is independently measured, the implied spread change is
`0.76 / 0.16`:

```
spread multiple during the flagged hour:  median 4.47×   (min 1.54×, max 9.07×)
```

So the ratio collapsed because **the spread widened ~4.5×**, not because the
range shrank. That is a restatement of the thing the hour was flagged for in the
first place — it is not independent evidence about candle quality.

**Nothing left standing.** Persistence is flat-to-higher, ranges are *smaller*
(0.76× ATR), and the one dramatic number is just the spread.

---

## What the data actually says the hour is

Not fake — **quiet and expensive**:

- **smaller ranges** (0.76× normal ATR)
- **~4.5× wider spreads**
- moves that **persist as well as any other hour** (0.98 / 1.11)

That is a liquidity thinning, and it has a precise cost signature: the *spread*
is the problem, not the *print*. A trade entered on such a bar pays ~4.5× the
normal round-trip on a bar with ~0.76× the normal room to work with.

---

## Implication for `suppress_on_spread_hour`

**The stated justification is wrong, and the response is mistargeted.**

Suppression throws the *signal* away. But the signal is fine — a break-and-close
on one of these bars is as likely to persist as on any other bar. What's wrong is
the **execution cost**, and that is exactly what the `max` SL floor in
`SCOPING-order-control.md` §4b-i already handles:

```
sl_distance = max(10 × last_candle_spread, 10 × expected_hour_spread, desired_sl)
```

A 4.5× spread flows straight into that floor, widening the stop, shrinking R, and
letting the ≥1R test park the trade **when the setup can't carry the cost** — and
only then. A wide-stop setup with a distant TP trades straight through; a tight
scalp is parked. That's the per-trade discrimination a boolean gate can't express.

**So this experiment removes the objection I raised** when the operator proposed
dropping `is_spread_hour` entirely. I argued suppression answered a
*signal-validity* question that the 1R filter couldn't reach. The data says there
is no signal-validity problem to answer — the bars are ordinary bars with an
expensive spread.

### Recommendation

`suppress_on_spread_hour` can retire along with the rest of `is_spread_hour`'s
order-control role (slice 7), **subject to the fixture A/B** — this study
establishes the bars aren't fake, not that removing the gate is P&L-positive.
The A/B is the confirming test.

### Caveats

- **OANDA H1 only.** TradeNation has a much bigger corpus (54M H1 rows) and
  spreads behave differently there (LOCKED note: OANDA EUR/USD 21:00 peak 1.81×
  vs TN 5.58×). Worth re-running per-broker before acting.
- **92 of 98 flagged instruments carry a single mask bit** (local hour 17, NY
  close), so this is overwhelmingly a study *of the NY close*. The 5 multi-hour
  outliers are unexamined.
- **Suppression only fires on bars ≤ 1h**, so H4/D are unaffected either way.
- `retrace1` 1.24 and `wick_share` 1.12 lean weakly toward the folklore. They are
  consistent with thin liquidity producing slightly wickier bars — real, but an
  order of magnitude short of "the print is fiction".

---

## Method

Per bar, versus the same instrument's non-flagged bars:

- `persistence_k = |close(t+k) − open(t)| / range(t)`, k ∈ {1,2,3} — measured from
  the **open** (where the move began), so a bar that round-trips scores ~0 even if
  it closed where it opened.
- `retrace_1` — fraction of the bar's range given back by the next bar.
- `wick_share = 1 − |close−open| / range`.
- `range_over_spread = range / (ask_close − bid_close)`.
- `range_over_atr` — 24-bar trailing true-range control, so "this hour is busier"
  can't masquerade as "this hour is fake".

Confound control: everything is **within-instrument**, and the reversion metrics
are range-relative, so they are scale-free by construction — a fake print and a
real move of the same size still separate. Welford accumulators (a naive
sum-of-squares is not stable at 1.6M samples). Missing inputs yield `None`, never
a silent zero that would bias a mean toward the hypothesis.

Reproduce:

```sh
cargo run -p trade-control-cli --example spread_hour_candle_stats --release -- \
    --granularity h1 --max-instruments 100 --days 1460 --json /tmp/stats.json
```

## Still to run

1. **Fixture A/B** — corpus with `suppress_on_spread_hour` on vs off. The seam is
   one early-return inside `suppress_on_spread_hour_bar_seconds` (both public fns
   funnel through it; do **not** gate `is_spread_hour` itself or the stop-widen is
   disabled too and the A/B is contaminated). Use
   `--bless-baseline` / `--baseline` and read only the `news-off` cells (~16 of 31)
   — news-on cells re-read the calendar and aren't reproducible.
   Power is weak: 31 fixtures, 1-bit masks ⇒ 1–2 suppressed bars each. It will show
   *which trades flip*, not whether the policy is net-positive.
2. **Re-run on TradeNation** (54M H1 rows), where the spread behaviour differs.
3. **News windows as a third population** — the operator's hypothesis is that news
   bars are genuinely more volatile than spread-hour bars, which would justify
   keeping a real blackout for news while spread hours only need sizing. This study
   supports the second half; the first half is untested.
