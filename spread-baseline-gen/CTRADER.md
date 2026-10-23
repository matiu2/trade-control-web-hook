# cTrader demo spread calibration

These commands read the selected **demo** account and write local calibration
artifacts. They never place, modify or close broker trades. Live profiles are
rejected before connecting. The main program must already have a named Open API
profile such as `ctrader-demo`; FIX and CLI credentials do not provide paired
historical spreads.

Run from the `trade-control-web-hook` workspace:

```sh
cargo build --release -p spread-baseline-gen --bin generate-ctrader --bin bake-ctrader

# Authenticate and inspect the workload without fetching historical ticks.
target/release/generate-ctrader --account ctrader-demo \
  --out spread-baseline-gen/validation/ctrader-demo-90d-report.json --plan

# All enabled, recognized broker spot-FX pairs; 90 calendar days by default.
target/release/generate-ctrader --account ctrader-demo \
  --out spread-baseline-gen/validation/ctrader-demo-90d-report.json

# Resume the original account, symbols and dates. Completed symbols survive;
# interrupted symbols reuse their already persisted cache windows.
target/release/generate-ctrader --account ctrader-demo \
  --out spread-baseline-gen/validation/ctrader-demo-90d-report.json \
  --resume spread-baseline-gen/validation/ctrader-demo-90d-report.json

# Verify broker identity and current native symbol IDs/pip units, then render
# the shared runtime table. No historical download occurs during baking.
target/release/bake-ctrader --account ctrader-demo \
  --report spread-baseline-gen/validation/ctrader-demo-90d-report.json
```

`--only EURUSD,AUDUSD` restricts a fresh run to exact native broker names.
`--from` and `--to` accept RFC3339 UTC timestamps; explicit starts must be minute
aligned, ends are rounded down to a completed minute. `--from` overrides `--days`.
A run is bounded to 366 days and `--window-hours` accepts 1–24, default 24.
Existing report paths require `--resume`; new reports cannot accidentally replace
an unrelated checkpoint. With `--resume A.json --out B.json`, an existing B is
refused unless it is the same canonical regular file as A. Symlink report paths
are refused. Output and resume source use process-scoped advisory locks; a second
calibration cannot write the same report concurrently. Empty `.json.lock` sidecars
remain on disk so locking uses a stable inode; their OS locks release even when
a process crashes. Atomic temporary report names include the process ID.

## Sampling and workload

Every measurement uses genuine paired BID/ASK/MID M1 candles from
`ChartData::cached_feed().range_bid_ask()`. The shared cache handles coverage and
preserves actual broker opening times. The historical source reconstructs paired
quotes from BID and ASK tick histories and checks native BID bars; this generator
never substitutes native BID candles or an estimated spread.

Daily windows amortize two initial book seeds per window. For one symbol over
90 days the nominal unpaginated cold workload is 630 historical requests:
90 × (three 500-minute native windows + two seeds + two quote-side ranges).
Actual tick pagination and backward seed search add requests; weekends, closed
sessions and warm cache coverage reduce them. This is a workload estimate,
not a runtime guarantee. Windows and instruments run sequentially through the
existing source request gate. Every completed window is cached and logged.

The report records account profile, demo endpoint/environment, Open API account
ID, broker name and terminal login from the authenticated trader response.
Each instrument records its native symbol ID, actual broker-native pip size,
instrument-lookup market-local schedule/timezone and measurement
bounds. Default discovery lists instruments outside recognized broker spot FX as exclusions;
transient metadata/network failures abort catalogue discovery instead of being
silently classified as unsupported.

Broker-native pip conventions may differ from the generic instrument catalogue
(for example EURCZK, EURHUF and GBPZAR). Calibration logs that difference and
uses the account's actual pip unit, matching cTrader entry/spread comparisons.
Prices, spread fractions and schedule-hour statistics keep their original scale.
The baker re-reads native metadata and rejects a changed symbol ID, name or pip
unit before writing the table. Existing successful report rows remain compatible.

## Promotion and uncovered symbols

Baking requires at least 90 calendar days, the existing spread quality thresholds,
finite ordered percentiles, matching hour masks/forecasts and unchanged schedule,
identity and broker-native pip metadata. The generated table uses account-scoped keys such as
`ctrader:ctrader-demo:EURUSD`; demo evidence never becomes a live-account default.
The reducer is shared with the MT5 measurement path, so its statistical gates
remain the same.

Failed symbols retain their reasons in the report. Strict baking refuses any
failure or unattempted symbol. An explicit `bake-ctrader --allow-partial` may
promote reviewed symbols from a fully attempted run; failures and insufficient
data remain uncovered, with a warning. It cannot promote an interrupted checkpoint
with unattempted symbols and never lowers a quality threshold. Existing tables
with another account's rows are refused rather than overwritten.

Baking changes source data only. Build and deploy consumers separately to activate
the new rows. No estimated spread is installed while calibration is incomplete.


## Add palladium without repeating the FX collection

The default universe and `finish-ctrader` remain FX-only. Explicit
`--only XPDUSD` also permits read-only palladium spread calibration using its
exact enabled broker symbol, native ID and native pip precision. No other
commodity class is enabled by this exception, and no contract sizing or live
non-FX order support is implied.

Use a **new report**, pinned to the same dates as the completed FX table:

```sh
target/release/generate-ctrader --account ctrader-demo --only XPDUSD \
  --from 2026-07-24T16:26:00Z --to 2026-10-22T16:26:00Z \
  --out spread-baseline-gen/validation/ctrader-demo-xpdusd-90d-report.json

# Add only the new measured row; retain the 54 FX row strings byte-for-byte.
target/release/bake-ctrader --account ctrader-demo --merge \
  --report spread-baseline-gen/validation/ctrader-demo-xpdusd-90d-report.json
```

The original report is never rewritten. The same genuine paired M1 history,
90-day minimum, statistical quality gates and fresh broker identity/pip checks
apply to palladium. An incomplete or insufficient report is refused.

`--merge` requires matching verified identity and sampling-window headers, adds
only previously absent keys and refuses duplicate replacement. It cannot combine
with `--allow-partial`. Both ordinary and additive bakes share an advisory output
lock; its empty `.rs.lock` sidecar remains for stable lock identity. The source
guard excludes this exact generated lock artifact alongside the generated table.

An ordinary replacement also refuses to remove any already measured symbol.
After palladium is added, a later FX-only table cannot silently erase it: a full
replacement report must include every existing profile, while `--merge` is only
for new symbols. Rebuild and deploy consumers after the measured row is added.
