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
Without `--stocks`, explicit selection uses the 93 audited native non-stock
mappings in `broker_ctrader::catalogue`, each with a verified spread schedule.
`--stocks` selects the separate audited 350 stock/ETF catalogue for `ctrader-demo`;
combine it with repeated `--only 'EXACT NATIVE NAME'` for a subset. Unverified
aliases are refused, even if a generic catalogue recognises the name. These
read-only selections do not enable broker order sizing or live trading.
`--all-instruments` explicitly discovers enabled, recognized **non-stock**
markets for this rollout. Stocks, missing mappings and missing/unknown schedule
timezones remain explicit exclusions; no class-derived calendar is guessed.
This flag cannot combine with `--only` or `--resume`. Resume uses the original
report's fixed symbol list and sampling dates.
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


## Add measured instruments without repeating the FX collection

The default universe and `finish-ctrader` remain FX-only. Explicit `--only`
accepts audited native non-stock names with verified schedule metadata, including
Gold (`XAUEUR`), indices, energy markets, crypto and FX. The same exact enabled broker name,
native ID and native pip checks apply to every non-FX instrument; FX additionally
retains broker currency-leg validation. No contract sizing or live non-FX order
support is implied. A class being supported does not mean that every native
broker name has a trustworthy catalogue mapping or schedule yet.

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
apply to every class. An incomplete or insufficient report is refused.

For Gold/EUR, use `--only XAUEUR` and a new report such as
`ctrader-demo-xaueur-90d-report.json`, preserving the table's UTC sampling bounds.
For a non-stock batch, obtain the ready native names from catalogue inventory and
subtract already measured account keys, then pass that explicit list via
`--only`. This avoids downloading the 55 completed profiles again. The direct
baker's `--merge` handles missing-only batch reports; `finish-ctrader` intentionally
continues to require the entire default FX universe and is not a missing-only
batch finisher.

`--merge` requires matching verified identity and sampling-window headers, adds
only previously absent keys and refuses duplicate replacement. Verified printable
ASCII native names retain their punctuation and spaces; keys are JSON-quoted
and stay scoped to their exact account. It cannot combine
with `--allow-partial`. Both ordinary and additive bakes share an advisory output
lock; its empty `.rs.lock` sidecar remains for stable lock identity. The source
guard excludes this exact generated lock artifact alongside the generated table.

An ordinary replacement also refuses to remove any already measured symbol.
After palladium is added, a later FX-only table cannot silently erase it: a full
replacement report must include every existing profile, while `--merge` is only
for new symbols. Rebuild and deploy consumers after the measured row is added.


## Durable missing-only non-stock completion

Build the dedicated runner beside the generator and baker:

```sh
cargo build --release -p spread-baseline-gen --bin generate-ctrader \
  --bin bake-ctrader --bin finish-ctrader-nonstocks

# Example only: replace the --only list with the actual missing native keys.
# Use a new batch report/status; existing matching reports resume automatically.
target/release/finish-ctrader-nonstocks --account ctrader-demo \
  --workspace . --only XAUEUR \
  --from 2026-07-24T16:26:00Z --to 2026-10-22T16:26:00Z \
  --report spread-baseline-gen/validation/ctrader-demo-missing-nonstocks-90d-report.json \
  --status spread-baseline-gen/validation/ctrader-demo-missing-nonstocks-status.json
```

Select exact native names from the current audited inventory and subtract the
latest table's account-qualified keys **after any ongoing bake finishes**. Quote
`--only` values containing spaces. The runner accepts audited non-stock names by default, or audited stock/ETF
names with `--stocks`, and explicit closed-minute bounds of 90–366 days. Before collection, it
reauthenticates the demo account and verifies selected symbols are enabled and
unambiguous, the existing table has the same identity and exact UTC window, and
every selected key is missing. Existing reports must match that identity, subset
and window before resume; unrelated reports are never selected for overwrite.

On completion it rechecks account/catalogue identity, requires every selected
profile to pass the strict quality gates, and invokes `bake-ctrader --merge`
without partial promotion. A report lock spans verification, merge and checks.
The original table bytes must remain unchanged during collection. Source and
dependency snapshots include existing drafts; any later source change stops
activation while retaining the report and cached windows. The runner reuses the
FX finisher's release tests, strict Clippy and formatting gates. It does not retry
failed collection automatically: fix the cause and rerun the same command to
resume. No Git writes or broker orders are performed.

Append `--deploy-staging` only when running from the staging branch to deploy
after all gates and verify the staging worker is active. Deployment is otherwise
absent. Status JSON records source hashes, progress and failure reasons; run the
command in a persistent user service for a long collection. The original
`finish-ctrader` continues to verify the full default FX universe.

If the additive merge succeeds but a later check or deployment fails, rerun the
same account, dates, `--only`, report and status arguments with `--verify-baked`.
This explicit recovery mode requires the complete exact-scope report and every
selected row already present **byte-for-byte** as freshly rendered from that
quality-checked report, with matching table identity and sampling window. It
reauthenticates the current demo account and verifies native symbol IDs and pip
metadata, then reruns the release/deployment gates under source and table guards.
It never invokes collection or baking, and accepts neither mixed present/missing
coverage nor an incomplete, stale or different report. Ordinary missing-only
mode continues to refuse already measured keys.


## Native stock and ETF collection

Stock history uses the exact demo broker name and ID, including spaces and
punctuation. Fresh native light/full symbol, asset, category and asset-class reads
must match the account-scoped audit before collection and again before baking.
This prevents aliases such as `AIA` from being mapped to a different company's
prices or currency. Native precision and pip units are retained. Close-only
instruments may supply history; no stock order permission or sizing is added.

```sh
# Inspect all 350 audited native stocks/ETFs, without downloading history.
target/release/generate-ctrader --account ctrader-demo --stocks \
  --out spread-baseline-gen/validation/ctrader-demo-stocks-90d-report.json --plan

# A fresh stock subset using the existing table's fixed 90-day bounds.
target/release/generate-ctrader --account ctrader-demo --stocks --only AIA \
  --only 'REDES ENERGETICAS NACIONAIS' \
  --from 2026-07-24T16:26:00Z --to 2026-10-22T16:26:00Z \
  --out spread-baseline-gen/validation/ctrader-demo-stock-subset-90d-report.json
```

Omit `--only` with `--stocks` for all 350. Resume with the original report and
`--resume`, without `--stocks`; the report's exact native list determines stock
scope. Stock and non-stock symbols cannot be mixed. The durable missing-only
runner keeps its historical `finish-ctrader-nonstocks` name and accepts `--stocks`
with an explicit missing stock list, forwarding stock selection to the generator
and retaining strict additive baking and all release gates.

All stock observations pass through the same paired candle-cache path. The
broker explicitly declares UTC sessions; `utc` is the spread learning clock,
not an inferred exchange timezone or historical holiday calendar. Current weekly
sessions do not filter historical candles. Sparse daily sessions retain the
existing minimum observation and hour-bucket thresholds; a measured baseline
with `MaskNotComputable` remains eligible, while insufficient history fails.

## Audited crossed-minute exclusions

Normal historical feeds remain strict. A calibration can explicitly exclude
whole M1 candles containing a finite, positive crossed book (`bid > ask`):

```sh
target/release/generate-ctrader --account ctrader-demo --only ETHEUR \
  --from 2026-07-24T16:26:00Z --to 2026-10-22T16:26:00Z \
  --quarantine-crossed-minutes --max-quarantined-minutes 5 \
  --out spread-baseline-gen/validation/ctrader-demo-etheur-quarantine-90d-report.json
```

The absolute limit defaults to five and accepts one through 100 distinct minutes
per instrument over the entire sampling window. Every completed profile must also
keep `excluded_minutes / (retained_M1_observations + excluded_minutes) <= 0.001`
(0.1%); collection, resume validation and strict baking enforce the same quota.
A larger explicit allowance cannot bypass this fraction for sparse stock sessions.
Existing reports retain their original policy; changing the allowance requires a
fresh report and leaves the original report and audit intact. Every exclusion
is durably recorded before collection continues, including native identity, minute bounds, observation timestamp and
bid/ask. Other data errors remain fatal. Valid segments use candle-cache;
excluded minutes remain uncovered gaps rather than cached empty results.

Keep the report's `.quarantine.json` sidecar: resume and baking validate its
identity, exclusions and digest. Resume requires the same explicit policy and
retains the original cumulative budget, including exclusions for unfinished
symbols. Start a fresh report when opting in for an earlier strict failure;
existing strict reports cannot silently acquire a different policy. The
missing-only finisher accepts and forwards both flags, and baked rows retain
an exclusion-count and audit-digest comment.
