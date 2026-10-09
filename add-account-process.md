# Adding a trading account

This records the places touched when connecting `the5ers-competition` on
2026-10-09. Use it as the starting checklist for the next account. Account
credentials, account metadata, CLI history and candle-cache identity are
separate stores.

## Choose the environment first

On this machine, plain `trade-control-accounts list` reads
`~/.config/trade-control/trade-control.toml`, pointing at `trade_control_dev`.
Staging uses `~/.config/trade-control/staging-worker.toml`, pointing at
`trade_control_staging`. Adding an account to one does not add it to the other.

`--database-url` / `DATABASE_URL` take precedence over `--config`. Without
either, the operator CLI searches the home configuration before the working
directory. Inspect the selected database before writing account metadata.

## Persistent locations

| Location | Purpose | What we changed for MT5 |
|---|---|---|
| `~/.config/trade-control/mt5-accounts.toml` | Named MT5 profiles | Added `the5ers-competition`, pinned login/server/clock and absolute credential/mailbox paths. `MT5_ACCOUNTS_FILE` can override this file. |
| `.mt5-creds.toml` in this checkout | Private WebTerminal credentials | Reused the existing `[creds] username/password` file; ignored it in Git. Never copy credentials into documentation or PostgreSQL metadata. |
| MT5 under Bottles, `MQL5/Experts/CandleBridge` and `MQL5/Files/mt5-read-only` | Historical tick access | Kept the compiled EA attached to one chart. One attachment serves requests for all instruments. The named profile points to the existing mailbox. |
| PostgreSQL `accounts`, database `trade_control_staging` | Staging broker routing and caps | Added the named MT5 demo/competition account. |
| PostgreSQL `accounts`, database `trade_control_dev` | Default operator account listing and dev routing | Added the same named account after updating the active dev worker to understand `mt5`. |
| `~/.config/trade-control/history.yaml` | Local account suggestions and `build-trade` known-account validation | Added the account name, preserving existing prep/veto/account history. Under `XDG_CONFIG_HOME`, use that configuration root instead. The native account-management CLI currently does not refresh this file. |
| PostgreSQL candle database `candle_cache` | Persistent candle history | Filled `candle_cache_mt5` (mid) and `candle_cache_mt5_bid_ask` (bid/ask/mid); `candle_cache_mt5_identity` pins the provider login/server. |
| `~/.cargo/bin/*-staging` | Installed arming, journal and replay tools | Rebuilt `trade-control`, `tv-arm`, `journal`, `replay-candles` and `tv-news` with the staging suffix. |
| `~/.cargo/bin/trade-control-accounts` and `trade-control-broker-check` | Unsuffixed operator tools | Rebuilt these too; their database selection is runtime configuration. |
| `~/.local/bin/trade-control-worker-{dev,staging}` and matching systemd user services | Active server processes | Updated both native worker binaries; each retains its own runtime configuration and database. |
| `README.md` and this file | Repeatable operator instructions | Recorded setup, read-only validation, candle import and current limitations. |

## Register and verify metadata

Install a worker that understands the broker **before** writing its metadata
row. An older worker cannot deserialize `mt5` and may fail to read the entire
account index. Update operator tools and any arming/journal binaries used for
the selected environment too. Wait for `/health` after restarting; systemd's
`active` status can precede HTTP readiness.

For this competition account, `demo` includes MT5's contest account type:

```sh
# Default/dev account index
trade-control-accounts add the5ers-competition --broker mt5 --kind demo
trade-control-accounts list

# Staging account index
trade-control-accounts --config ~/.config/trade-control/staging-worker.toml \
  add the5ers-competition --broker mt5 --kind demo
trade-control-accounts --config ~/.config/trade-control/staging-worker.toml \
  get the5ers-competition
```

For an already registered account, use `get`; `add` rejects duplicates. Choose
per-account risk and position caps deliberately (`--max-risk-pct` and
`--max-open-positions`); omitted values inherit worker-wide caps. The MT5
implementation currently refuses real/live accounts.

## Update local recall

The metadata command does not populate the local history file. Include the
account in its `accounts` list, with a UTC `last_used` timestamp, preserving
other entries. The shared helper is
`trade_control_cli::history::record_account_use`; use it when implementing an
automatic registration workflow. A nonempty history missing the new account
can cause `build-trade --from-file` to reject an otherwise registered account.

## Validate without trading

```sh
trade-control-broker-check the5ers-competition \
  --config ~/.config/trade-control/staging-worker.toml \
  --instrument EURUSD --candle-count 3 --preview-minimum-short
```

This resolves the account through the worker's broker factory, reads a quote
and EA-derived candles, and previews a minimum-lot short with SL 20 pips and
TP 40 pips. It submits no orders. Use an instrument without existing exposure;
the adapter refuses a new entry on an instrument with positions or pending
orders. Set `RUST_LOG=info` to see candle and sizing details.

An additional signed HTTP check was performed against staging with
`broker: mt5`, `account: the5ers-competition`, a unique `id` / `trade_id`, and
`dry_run: true`. Use the selected environment's signing key and valid shell
fields; omit unused signal fields as `null`. The server log confirmed
`DRY-RUN ... (not placed)`. Do not turn this check into a live entry test.

## Populate the candle cache

From the trading-libraries checkout:

```sh
cargo run --manifest-path mt5-data-source/Cargo.toml --bin mt5-candles -- \
  --account the5ers-competition --symbol AUDUSD --timeframe H1 --count 300
```

Keep MT5 and CandleBridge running. History is built from paired historical
bid/ask ticks; minute bid OHLC must match native MT5 bars before caching. Mid
OHLC is built from tick mids. Download failures or incomplete history fail
explicitly; retry after the terminal has downloaded the requested history.

`MT5_CACHE_DATABASE_URL` selects the candle database independently of the
worker state database. The current MT5 cache supports one pinned login/server
per database. A second MT5 profile needs account-specific cache routing or a
separate worker/candle database; do not erase the identity row to mix feeds.
Update the explicit broker clock if the broker changes its UTC offset.

## Arming and replay

Select `--broker mt5 --account-id the5ers-competition` with `tv-arm-staging`.
MT5 has no baked spread forecast yet, so arming currently needs the explicit
`TV_ARM_ALLOW_UNBAKED=1` override; live and historical spread checks remain.
Saved MT5 plans replay through `journal-staging` using the account in their
intents. Standalone replay can use
`--source mt5 --mt5-account the5ers-competition`.

## Adding an account versus adding a broker

Another account for an existing broker usually needs configuration,
credentials, selected database metadata and local recall. A new broker also
needs code integration: core/conventions broker enums and symbol mapping,
the `Broker` implementation, native HTTP and cron factories, operator CLI
arguments, `tv-arm` quote/instrument/spread paths, and replay candle source
selection. Rebuild consumers before registering the new broker's first row.

The MT5 integration lives in `broker-mt5/` and the sibling `mt5-data-source/`.
Its code commits are `f9eb0cf3` and `6232a177`. Validation passed 3,578 workspace
tests and strict Clippy; five existing oil replay golden mismatches were
reproduced identically on the pre-integration code. Both account indexes and
staging's signed dry-run route were checked after installation.

After the default-index registration, both `trade-control-accounts list` and
`trade-control-staging account names` returned `the5ers-competition`. Both
worker health endpoints passed. The nine CLI history tests, formatting check
and strict workspace Clippy check passed for this follow-up.
