# Why TradeNation indices/commodities won't bake into the spread table

**Found:** 2026-09-16, chasing `tv-arm-staging --spec-in ...` refusing Spain 35
with *"instrument \"Spain 35\" is not in the baked spread table (no row)"*.

## What is actually missing

Not just Spain 35. The baked table has **160 rows: 125 OANDA + 35 TradeNation**,
and every TN row is FX or a spot metal. **No TN index, commodity or crypto is in
the table at all** — 39 instruments, of which 38 are bakeable (`ASTRAZENECA` has
`spread_schedule = "none"`, which the generator skips by design).

So Spain 35 is not a one-off: UK 100, Germany 40, Wall Street 30, US 500, Bitcoin,
Coffee … all hit the same refusal the moment you arm them.

## Two dead ends, ruled out with evidence

**Not the `Tradable` flag.** `Spain 35` reports `tradable: false` while Germany 40
reports `true`, which looked causal. It isn't — the operator flagged it as a known
red herring on TradeNation, and `tradenation instruments list` lists Spain 35 as a
normal market.

**Not missing bid/ask data.** `market-info --market-id 66670` reports a live
spread of 3, and the adapter itself returns a clean spread on every bar:

```
Spain 35   — 11281 M1 candles, 0 with ask_c == bid_c   (spread = 3.0 throughout)
Germany 40 — 30122 M1 candles, 0 with ask_c == bid_c   (spread = 1.0 throughout)
```

The data reaching the baker is correct. An earlier reading of `vol=0.000000` as
"TradeNation collapses bid/ask for this market" was **wrong** and is recorded here
so nobody re-derives it.

## The actual cause: a 24h-market assumption in `apply_gates`

`compute.rs::apply_gates` opens with a bare literal:

```rust
let ratios: Vec<f64> = flag_ratio.iter().filter_map(|r| *r).collect();
if ratios.len() < 12 {
    return SpreadProfile::empty(n_bars);   // vol: 0.0, forecast all-zero
}
```

`ratios` holds one entry per **local hour that cleared `MIN_HOUR_MINUTES` (20)**.
A part-time exchange cannot reach 12 such hours no matter how much history is
fetched, so it always returns the empty profile — which zeroes `vol` and the whole
row, *including* the healthy volatility series it just computed.

Measured through the real `profile_from_minutes`:

| instrument | session (London) | sampled local hours | vol |
|---|---|---|---|
| Spain 35 | 08:00–16:30 | **9** | 0.00000000 |
| Coffee | 09:15–18:30 | **10** | 0.00000000 |
| Euro Stocks 50 | 07:00–21:00 | 14 | 0.00093697 |
| Germany 40 | ~24h | 23 | 0.00068282 |

Spain 35's vol series is *healthy* — 199 hourly returns, none degenerate. The
`< 12` gate discards it anyway.

## Blast radius: 6 of 36 instruments, and a silent-degrade hole

Sessions shorter than ~12h — **Spain 35 (8.5h), South Africa 40 (9h), Cocoa
(8.75h), Coffee (9.25h), Orange Juice (6h), Sugar No 5 (9.25h)** — can never bake
under the current rule. The other 30 have 12+ hour sessions and bake normally
(verified on Germany 40, UK 100, Euro Stocks 50).

**A naive "just bake it" makes things worse, silently.** A row written from an
empty profile is `reviewed = false` with widen AND forecast all-zero. But
`spread_blackout::coverage()` destructures `reviewed` and never reads it:

```rust
let (_broker, _symbol, schedule, _reviewed, _mask, widen, _m, _l, _h, forecast) = row;
...
if forecast.iter().all(|f| *f <= 0.0) && widen.iter().any(|w| *w > 0.0) {
    return Coverage::StaleForecast;
}
Coverage::Covered
```

Flat-on-both is treated as a legitimate "reviewed, genuinely no spread hour"
verdict, so such a row reads **`Covered`** — the arm gate passes and the stop is
sized with no spread forecast at all. That is exactly the silent degrade the
refusal at `instrument_resolution.rs:153` exists to prevent, reintroduced through
the back door.

## Also worth knowing: `--only` is a trap

The refusal's own advice —

```
cargo run -p spread-baseline-gen --bin generate -- --brokers <...> --days 90 --only "Spain 35"
```

— would **destroy the table**. `render_table` writes only the rows passed to it,
so an `--only` run emits a 1-row file and the other 159 rows are lost. The Aug-02
TN re-bake (`663faf0c`) worked around this by baking in batches and splicing the
result; there is no script for it in the repo.

Two further wrinkles for whoever does the bake:
- `"Spain 35"` matches **two** catalog ids (`ES35` and `SPAIN35`), so a bake emits
  two identical rows; `render_table` dedups on `(broker, symbol)`.
- The generator hangs on a large fan-out (noted in `b13f34e`, still unfixed), so
  batches of ~8 with a timeout and retry are the known-good approach.

## What needs deciding

1. **The `< 12` rule.** It is unnamed, uncommented, and encodes "markets trade
   ~24h". A part-time session with 9 well-sampled hours is not low-confidence
   data — it is a complete picture of a shorter day. Options: lower the floor,
   scale it to the instrument's actual session length, or make it a named
   constant with an explicit part-time branch.
2. **Close the `coverage()` hole** regardless of (1): an unreviewed all-zero row
   must not read as `Covered`. Reading the `reviewed` column it already
   destructures would do it.

Neither should be decided by whoever is merely trying to arm one chart.
