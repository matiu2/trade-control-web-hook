# EXPERIMENT — is a spread-hour candle actually "rubbish"?

**Run:** 2026-07-31 · OANDA + TradeNation H1
**Sample:** OANDA 66 instruments / **71,351** flagged vs **1,564,202** control bars (4y)
· TradeNation 32 instruments / **20,638** flagged vs **375,956** control bars (3.3y)
**Tool:** `cli/examples/spread_hour_candle_stats.rs` (`--broker oanda|tradenation`)
**Raw:** `/tmp/claude-1000/spread-hour-stats.json`, `/tmp/claude-1000/tn-stats.json`

---

## Result: the folklore is NOT supported

The engine suppresses signals on spread-hour bars at 5 sites in
`engine/src/evaluate.rs`, justified by the inherited claim that the print
"doesn't reflect real price" — internal broker matching or an absent liquidity
provider, so the OHLC is an artefact of who was quoting.

That claim predicts the move **should not stick**. Measured against the same
instrument's other hours (OANDA figures; TradeNation in the next section agrees):

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
spread multiple during the flagged hour:
  OANDA        median  4.45×   (max  9.07×)
  TradeNation  median 11.01×   (max 18.09×)   <- market-maker, far worse
```

So the ratio collapsed because **the spread widened** (4.5× on OANDA, 11× on TN),
not because the range shrank. That is a restatement of the thing the hour was flagged for in the
first place — it is not independent evidence about candle quality.

**Nothing left standing.** Persistence is flat-to-higher, ranges are *smaller*
(0.76× ATR), and the one dramatic number is just the spread.

---

## Both brokers agree — TradeNation just costs 2.5× more

The operator flagged TN as having "humungous spreads at 7am" (= 17:00 NY, the
same flagged hour). Confirmed, and then some:

| metric (median ratio vs same instrument's other hours) | OANDA | TradeNation |
|---|---|---|
| `persist1` — move alive 1 bar later | 0.98 | **0.94** |
| `persist3` — alive 3 bars later | 1.11 | **1.09** |
| `retrace1` | 1.24 | 1.12 |
| `wick_share` | 1.12 | 1.15 |
| `range_over_spread` | 0.156 | **0.082** |
| `range_over_atr` | 0.76 | **0.88** |
| **implied spread multiple** | **4.45×** | **11.01×** |

Worst TN offenders: EUR/GBP **18.1×**, EUR/JPY 14.8×, GBP/AUD 13.2×, AUD/USD
13.2×, USD/CAD 13.0×, EUR/AUD 12.9×.

**The qualitative conclusion is unchanged, and TN strengthens it.** Persistence
is flat on both brokers (0.94 / 1.09 on TN — moves stick just as well), and TN's
ranges are *closer* to normal than OANDA's (0.88 vs 0.76 × ATR). The entire
difference between the brokers is **execution cost**: TN's spread blows out 2.5×
harder than OANDA's at the same hour.

That is precisely the split the `max` SL floor is built for. An 11× spread is a
sizing input, not a reason to disbelieve the candle — and because the floor is
per-instrument and per-broker (the baked table is keyed
`(broker, symbol)`), EUR/GBP on TN at 18× is automatically treated far more
conservatively than the same pair on OANDA, with no separate rule.

A boolean gate cannot express that. It suppresses both identically, despite one
being 2.5× more expensive than the other.

## What the data actually says the hour is

Not fake — **quiet and expensive**:

- **smaller ranges** (0.76× normal ATR)
- **4.5× (OANDA) to 11× (TN) wider spreads**
- moves that **persist as well as any other hour** (0.98 / 1.11)

That is a liquidity thinning, and it has a precise cost signature: the *spread*
is the problem, not the *print*. A trade entered on such a bar pays ~4.5× the
normal round-trip — 11× on TradeNation — on a bar with less room than usual to
work with.

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

- ~~OANDA H1 only~~ — **both brokers now measured** (see above). TN is 2.5×
  more expensive but qualitatively identical; the conclusion holds on both.
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
# OANDA
cargo run -p trade-control-cli --example spread_hour_candle_stats --release -- \
    --broker oanda --granularity h1 --max-instruments 100 --days 1460 --json /tmp/oanda.json
# TradeNation (needs a demo session; reads the TN cache)
cargo run -p trade-control-cli --example spread_hour_candle_stats --release -- \
    --broker tradenation --granularity h1 --max-instruments 40 --days 1200 --json /tmp/tn.json
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
2. **News windows as a third population** — the operator's hypothesis is that news
   bars are genuinely more volatile than spread-hour bars, which would justify
   keeping a real blackout for news while spread hours only need sizing. This study
   supports the second half; the first half is untested.
