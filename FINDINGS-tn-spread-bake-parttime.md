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

## Postscript: two dead catalog entries (found during the bake)

`NEO` and `DASHUSD` name TradeNation symbols (`"NEO"`, `"DASHCUSD"`) that do
**not exist** in TradeNation's market list — not a fetch failure, not a rename:
`tradenation instruments dump` has no market by either name, nor anything
similar. TradeNation appears to have delisted both coins.

Both are TN-only crypto ids (`oanda = ""`), so they are unreachable on either
broker and cannot be armed or baked. They are skipped by the bake for that
reason, which is why it covers 36 instruments rather than 38.

Nothing here depends on fixing them — noting it so the next person doesn't
re-diagnose it as a flaky fetch. The catalog entries should either be removed
or have their `tradenation` field emptied.

## Postscript 2: Germany 40 misses its spread hours by 0.6%

Worth recording because the row will read "no spread hour" and that is NOT the
same claim as "this market has a flat spread".

Germany 40 has a clean, consistent **two-tier** spread — ~6x wider off-session
than in the cash session:

```
--- tradenation Germany 40  vol=0.000680  med_ratio=0.114  threshold(3x)=0.342 ---
  hours 0-7, 22   ratio 0.34    <- off-session, wide (0.000235)
  hours 9-16      ratio 0.06    <- cash session, tight (0.000039)
  hours 17-21, 8  ratio 0.11
```

Nine hours sit at **0.34** against a `MED_MULT x median` threshold of **0.342**.
They miss by 0.6%, so the mask bakes empty. A slightly different 90-day window
would flip all nine on. The verdict is a coin-flip, not a measurement.

**This is not caused by the mask/forecast split.** Verified: Germany 40 baked
`mask = 0` with the pre-split binary too. The split only adds an early return
for sessions below `MIN_MASK_HOURS`; Germany 40 has 23 sampled hours and never
takes that path. `a_full_day_market_still_flags_its_spike` covers the
regression.

**Why UK 100 flags and Germany 40 doesn't**, on near-identical 6x variation:
UK 100 has a *third*, tighter tier (0.00007 at hours 11-14) that drags its
median down to 0.000094, so its wide band clears 3x comfortably. Germany 40 is
cleanly two-tier, so its median sits nearer the wide band and the ratio
compresses. This is the three-tier dynamic `PEAK_FRAC` was introduced for, seen
from the other side: there the concern was a low median letting a benign band
through; here a high median keeps a real band out.

**Consequence.** The forecast column still reports the true per-hour cost, so
the forward-looking SL floor is correct either way. What is lost is the
mask/widen: the System-2 stop-widen never fires on Germany 40's genuinely wide
off-session hours, and `is_spread_hour` falls back to the NY-close-edge
default.

**Not changed here.** `MED_MULT` is load-bearing for all 160 rows and was
calibrated against a full sampler + OANDA audit; retuning it for one instrument
is its own change with its own evidence, not a side effect of a bake. Flagged
for a decision.

Of the 16 rows baked so far, Germany 40 is the ONLY marginal one — the other 15
have peak/median ratios of 1.00-1.75 against a 3x threshold and are flat by a
wide margin.

## Postscript 3: the local overlay silently strips `spread_schedule` from 22 assets

**This is a live config bug, not specific to this bake.** It is why Bitcoin,
Stellar and TRON could not be baked, and it would quietly degrade three
already-good committed rows on the next re-bake.

`~/.config/instrument-lookup/mappings.toml` is merged over the baked-in
catalog, and per the parent CLAUDE.md an overlay block with the same `id`
**replaces** the baseline entry — *"The overlay block is the whole replacement —
omitting a field is not inheritance."*

22 of the overlay's 37 asset blocks omit `spread_schedule`. Those assets
therefore resolve to `"none"` regardless of what `catalog.toml` says:

```
catalog.toml:  id=BITCOIN  tradenation="Bitcoin"  spread_schedule='ny'
mappings.toml: id=BITCOIN  tradenation="Bitcoin"  (no spread_schedule)
=> resolved:   spread_schedule = "none"
```

`spread-baseline-gen` skips a `none` schedule by design (it has no spread hour
to bucket), so:

```
tradenation Bitcoin: schedule 'none' has no spread hour — skipped
tradenation Stellar: schedule 'none' has no spread hour — skipped
tradenation TRON:    schedule 'none' has no spread hour — skipped
```

Earlier in this investigation these three were recorded as transient fetch
failures, on the evidence that the markets exist and their candle feeds are
healthy (they do, and they are). That diagnosis was **wrong** — the generator
never attempted a fetch. The distinguishing evidence is the log line above,
which only appeared once they were run individually rather than inside a batch.

**Blast radius beyond the three crypto.** Three rows already in the committed
table would lose their schedule if re-baked today:

| row | schedule now | after a re-bake |
|---|---|---|
| oanda `FR40_EUR` | `frankfurt` | `none` |
| oanda `HK33_HKD` | `hongkong` | `none` |
| tradenation `Spot Silver` | `ny` | `none` |

A `none` schedule makes the row inert: `coverage()` returns `NoSchedule` and the
mask cannot be indexed. None of the 33 rows baked in this run carry
`schedule = "none"` — verified — because the affected instruments were skipped
rather than baked badly. So this bake is safe to merge; the bug is a trap for
the *next* one.

**Fix:** add `spread_schedule` to the 22 overlay blocks that omit it, copying
the baseline value (`instrument-lookup resolve <id>` prints it). Not done here —
it is an edit to the operator's machine-local config, outside this repo, and
should be a deliberate change rather than a side effect of a bake.
