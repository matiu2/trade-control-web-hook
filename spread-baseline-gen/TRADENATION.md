# Complete TradeNation spread bake

The embedded `instrument_lookup::tradenation` snapshot contains every distinct
`TradeOnWeb` market, including stocks, fixed/variable spread variants and dated
contracts. Trading identifiers and quotes are still resolved from the broker.

Refresh it from a fresh broker walk (a normal CLI dump can reuse its cache):

```sh
python3 instrument-lookup/scripts/bake_tradenation_catalog.py /tmp/fresh-tradenation-markets.json
```

From the trade-control workspace, collect paired M1 books through the shared
Postgres cache and merge successful profiles into the shared spread table:

```sh
DATABASE_URL=postgresql://candle_cache:candle_cache@localhost:5432/candle_cache \
  cargo run -p spread-baseline-gen --bin generate-tradenation -- --concurrency 4
```

The checkpoint fixes the requested 60-day window. Many stocks and newly named
markets expose only about a month of M1 history. Each accepted profile must pass
the existing minute-profile sufficiency check, contain a positive finite spread
forecast, include ten observed dates across more than a week, and have data
within the last seven days of the requested window. The report records actual
first/last observations and date counts. Short sessions retain their measured
forecasts even when their hours cannot be ranked for a blackout mask.
Sparse stock histories require 20 samples per learned hour and the same
date/span/recency checks. When their
hourly volatility cannot be ranked, only the measured forecast is retained.

Rerun the same command to resume missing or failed instruments. `--limit N`
bounds one invocation. Catalogue additions/removals are reconciled on resume;
changed precision/schedules invalidate old measurements. Checkpoints older than
seven days are refused: use a new `--report` path for a fresh calibration.

The report and destination each have a process lock; writes are atomic. A merge
preserves other brokers and existing unmeasured rows. The command returns an
error until the entire current catalogue is measured, even when useful partial
results were merged. A listing with no usable history remains uncalibrated;
the arm-time spread coverage refusal remains in effect for it.

Rebuild and install the chart, worker, arming and replay binaries after updating
the catalogue or shared table: running binaries retain their compiled snapshot.
