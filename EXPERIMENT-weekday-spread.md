# EXPERIMENT — does day-of-week add anything to the hour-of-day spread forecast?

**Run:** 2026-08-01 · OANDA + TradeNation H1
**Sample:** OANDA 60 instruments / 1378 cells / **1,406,943** bars (4y)
· TradeNation 35 instruments / 840 cells / **431,750** bars (3.3y)
**Tool:** `cli/examples/spread_weekday_stats.rs`
**Raw:** `/tmp/claude-1000/weekday-{oanda,tn}.json`

---

## Result: NO — hour-of-day already captures it

The forecast column added in this branch bakes a p90 spread per **schedule-local
hour**, pooling all five weekdays. Two plausible mechanisms would break that
pooling: the **Friday NY close** (weekend risk-off thinning the book) and the
**Sunday reopen** (the weekend gap). If either were real, the pooled p90 would
mis-size every trade resting into that hour.

Scored **within each (instrument, local-hour) cell** — a weekday's spread
compared only against the same instrument at the same hour on other weekdays —
so the week's uneven hour mix can't masquerade as a weekday effect:

| weekday | OANDA median | p25 | p75 | TN median |
|---|---|---|---|---|
| Mon | 1.000 | 0.991 | 1.004 | 1.000 |
| Tue | 0.999 | 0.989 | 1.001 | 1.000 |
| Wed | 1.000 | 0.992 | 1.005 | 1.000 |
| Thu | 1.000 | 0.995 | 1.008 | 1.000 |
| **Fri** | **1.004** | 1.000 | 1.022 | **1.000** |
| **Sun** | **1.094** | 1.015 | **1.589** | **1.001** |

Ratio 1.00 = exactly typical for that instrument at that hour.

**Friday is a non-event.** 1.004 on OANDA, 1.000 on TN — four thousandths. The
weekend-risk-off story is not visible in the spread at all.

**Mon–Thu are indistinguishable** from each other to three decimal places on both
brokers.

## The one real finding: the Sunday reopen is a TAIL, not a shift

Sunday's OANDA median is only 1.09, but its **p75 is 1.589** — the distribution
is skewed, not displaced. That is the reopen effect showing up exactly where
you'd expect it: a minority of instruments gap badly on the reopen while most are
normal.

It is also a **thin, hour-restricted population** — 145 buckets vs Monday's 1378
(11%), because the Sunday session is only the ~2h reopen window. So the hours
that carry it are already distinct hours, which the existing per-hour bucketing
separates on its own.

TradeNation shows none of it (1.001, p75 1.010) — consistent with a market-maker
quoting its own book across the reopen rather than passing through interbank
thinness.

---

## Decision: don't add a weekday axis

Adding one would **divide every bucket's sample count by ~5** for a median effect
of 0.4% (Friday) to 9% (Sunday, one broker, thin population). Thinner buckets
make the p90 noisier, so the forecast would get *worse* at the 23 hours where
weekday demonstrably doesn't matter, to slightly improve one.

The Sunday tail is better handled by the machinery already being built:

- The **reactive** term of the SL floor (`10 × last_candle_spread`) sees an
  actual reopen blowout as it happens, without needing to have predicted it.
- The **`max`** means whichever term is worse wins, so a Sunday gap is covered by
  the measured reading even though the forecast under-predicts it.

That is precisely the split the `max` exists for: forecast the *predictable*
(hour-of-day), measure the *unpredictable* (a specific bad reopen).

## Also confirmed: DST is already handled

`spread-baseline-gen` buckets by **schedule-local hour**, not UTC — the NY-close
spike sits at local hour 17 year-round rather than smearing between UTC 21
(summer) and 22 (winter). There is a dedicated test asserting a January and a
July bar land in the same local hour (`fetch.rs::minute_bar_local_hour_tracks_ny_dst`).
The forecast column added here inherits that DST-invariance for free.

## Caveats

- **UTC hours, deliberately.** This probe buckets on UTC hour, not schedule-local
  — it asks whether weekday adds anything *given* hour bucketing, and applying a
  second timezone transform would confound the two questions. If a weekday effect
  had shown up, it would need re-measuring in local hours before being baked.
- **Saturday is absent** from both runs (no bars) — as expected.
- H1 only; a sub-hour reopen spike would be averaged into its hour.

## Reproduce

```sh
cargo run -p trade-control-cli --example spread_weekday_stats --release -- \
    --broker oanda --granularity h1 --max-instruments 60 --days 1460
cargo run -p trade-control-cli --example spread_weekday_stats --release -- \
    --broker tradenation --granularity h1 --max-instruments 40 --days 1200
```
