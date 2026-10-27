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
| `~/.config/trade-control/mt5-accounts.toml` | Named MT5 profiles | `trade-control-accounts add` / `credentials` maintain the pinned login/server/clock and absolute credential/mailbox paths. `MT5_ACCOUNTS_FILE` can override this file. |
| Profile's `credentials_file` (originally `.mt5-creds.toml` in this checkout) | Private WebTerminal credentials | The same commands write `[creds] username/password` with owner-only permissions on Unix. Existing paths are reused; new profiles default to `mt5-credentials/<name>.toml` beside the profile configuration. Never copy credentials into documentation or PostgreSQL metadata. |
| MT5 under Bottles, `MQL5/Experts/CandleBridge` and `MQL5/Files/mt5-read-only` | Historical tick access | Kept the compiled EA attached to one chart. One attachment serves requests for all instruments. The named profile points to the existing mailbox. |
| PostgreSQL `accounts`, database `trade_control_staging` | Staging broker routing and caps | Added the named MT5 demo/competition account. |
| PostgreSQL `accounts`, database `trade_control_dev` | Default operator account listing and dev routing | Added the same named account after updating the active dev worker to understand `mt5`. |
| `~/.config/trade-control/history.yaml` | Local account suggestions and `build-trade` known-account validation | Added the account name, preserving existing prep/veto/account history. Under `XDG_CONFIG_HOME`, use that configuration root instead. The native account-management CLI currently does not refresh this file. |
| PostgreSQL candle database `candle_cache` | Persistent candle history | Filled `candle_cache_mt5` (mid) and `candle_cache_mt5_bid_ask` (bid/ask/mid); `candle_cache_mt5_identity` pins the provider login/server. |
| `../candle-cache/src/spread_profiles/mt5.rs` | Shared expected-spread profiles | Bake reviewed 90-day calibration with `mt5:<account>:<native-symbol>` keys, then publish cache and rebuild consumers. See [spread baking](../candle-cache/SPREAD-BAKING.md). |
| `~/.cargo/bin/*-staging` | Installed arming, journal and replay tools | Rebuilt `trade-control`, `tv-arm`, `journal`, `replay-candles` and `tv-news` with the staging suffix. |
| `~/.cargo/bin/trade-control-accounts` and `trade-control-broker-check` | Unsuffixed operator tools | Rebuilt these too; their database selection is runtime configuration. |
| `~/.local/bin/trade-control-worker-{dev,staging}` and matching systemd user services | Active server processes | Updated both native worker binaries; each retains its own runtime configuration and database. |
| `local-chart` feed selector and `/feeds`, `/instruments?broker=…` | Chart discovery | Discovers each configured profile as `mt5-<account>`, fetches its authenticated symbol catalogue, and stores drawings under that feed key. No manual instrument JSON edit is needed. |
| `local-chart-client`, `tv-arm`, `journal`, `replay-candles` | Account identity through chart links and exports | Preserves exact MT5 symbols and the named account feed in exported specs, navigation, plan geometry and replay positions. |
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

### MT5 credentials and account changes

MT5 `add` now prompts for a hidden trading password and writes both the named
profile and its private credential file. Existing profiles supply defaults for
login, server, clock, mailbox and credential path. A new profile needs explicit
identity, server clock and CandleBridge mailbox settings:

```sh
trade-control-accounts add another-competition --broker mt5 --kind demo \
  --login 12345678 --server FivePercentOnline-Experience \
  --server-time +03:00 \
  --mailbox '/absolute/path/to/MetaTrader 5/MQL5/Files/mt5-read-only'
```

Use the account's verified current clock; the example offset is not permanent.
Pass `--credentials-file /absolute/path/to/private.toml` to select another
credential location. Passwords are entered at the prompt, never as command-line
arguments. Automation can supply one password line with `--password-stdin`;
spaces are preserved and the line terminator is removed.

To rotate a password or replace a competition login, update the existing account
without removing its metadata or risk caps:

```sh
trade-control-accounts credentials the5ers-competition --login 12345678
# For staging metadata, add the global config option:
trade-control-accounts --config ~/.config/trade-control/staging-worker.toml \
  credentials the5ers-competition --login 12345678
```

The `credentials` command checks the account in the selected database and
updates local files only. It accepts the same `--server`, `--server-time`,
`--mailbox` and `--credentials-file` overrides. Omit `--login` only when the
account number is unchanged. The pinned profile login and credential username
are written together. A credential file shared by another named profile is
rejected; select a separate file instead. Other profiles and their credentials
are preserved. The CLI serializes profile updates, stages file replacements,
and restores previous files on ordinary write/metadata errors; the file pair
and PostgreSQL are not a single crash-atomic transaction.

Restart `local-chart` and workers using the account after changing its profile
or credentials; running clients retain the previous identity. Also log desktop
MT5 into the matching account so CandleBridge can supply uncached paired history.
The CLI does not modify the desktop terminal's saved login or place any trades.
New login/server identities automatically use separate candle-cache namespaces.

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
worker state database. Each MT5 login/server has an isolated cache namespace. The original matching
account keeps `candle_cache_mt5`; additional identities use
`candle_cache_mt5_<login>_<server-hash>` with matching `_bid_ask` and `_identity`
tables. Do not erase identity rows or reuse another provider's cached candles.
Update the explicit broker clock if the broker changes its UTC offset.

## Arming and replay

Select `--broker mt5 --account-id the5ers-competition` with `tv-arm-staging`.
The chart exports `mt5-the5ers-competition:EURUSD`; `tv-arm` can infer its
broker and account when flags/environment do not override them. Journal
links use `broker=mt5-the5ers-competition`, while account metadata keeps the
broker kind `mt5`.

Reviewed account-specific forecasts are shared through candle-cache. Uncovered
instruments still require the explicit `TV_ARM_ALLOW_UNBAKED=1` override;
live and historical spread checks remain. Never copy another account's profile.
Saved MT5 plans replay through `journal-staging` using the account in their
intents. Standalone replay can use
`--source mt5 --mt5-account the5ers-competition`.
Offline replay defaults to `--mt5-history auto`: unavailable genuine ask/mid
may be estimated from native bid and the same reviewed median. Reports and
fixtures identify the estimate. `--mt5-history paired` requires genuine books;
`--mt5-history estimated` reads cache/web bid history without the EA. This does
not change live trading's requirement for real bid/ask.

Manual position-tool entries (`--market-entry`, `--stop-entry`, `--limit-entry`)
submit a direct signed intent rather than an engine plan. Their `plan-out` and
`replay` combinations are refused before any I/O. For a server check, use
`--broker-dry-run register`; a successful dry run submits no broker order.

The offline-command guard was checked for all three position entry types,
including a nonexistent spec file: rejection happens before reading the spec
or contacting the worker. All 565 `tv-arm` tests and strict Clippy passed,
including a worker response delayed beyond the previous 20-second deadline.
The CLI now waits up to 120 seconds for the result; it does not automatically
resend an entry after a timeout. A timeout can leave an uncertain result, so
inspect the server's request timeline before repeating a live entry.

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

## cTrader credentials and prop risk in one operation

Run the accounts CLI on the host whose worker reads the local profiles. Database
selection is independent (`--config`/`--database-url` still choose the account
index). The command creates local configuration; it does not deploy/restart
workers, place a trade, or change desktop platform credentials.

Interactive setup now handles the cTrader Open API account ID, hidden OAuth
credentials, optional FIX credentials and binding, prop policy, instrument costs,
and initial risk enrollment:

```sh
trade-control-accounts add my-prop-account --broker ctrader --kind demo \
  --prop-firm --ctrader-fix
```

Choose `demo`/`live` to match the actual broker environment; purchasing a prop
account does not determine that value. `--ctrader-fix` prompts for separate QUOTE
and TRADE endpoint details/passwords and the exact Open API broker name. This
configures the existing hybrid backend, currently demo-only. Without that flag,
Open API credentials can be registered and the existing restriction on
Open API-only market entries remains. Existing adapter connection/account
limitations are unchanged; registration does not claim connectivity or execution
readiness.

Existing FIX-only quote/management profiles can still be registered into another
account index without OAuth by omitting credential and risk flags. Prop protection
requires Open API account data.

The Open API uses application client ID/client secret/access token. A normal
cTrader username/password is not an OAuth access grant. Obtain/authorize the
application grant first; this command stores it and the existing cTrader API
validates it when connecting. `--ctrader-reuse <existing-profile>` reuses that
profile's application/grant for another explicitly selected account ID. The grant
must authorize that account. New account names must use letters, numbers, dashes
or underscores. The Open API account ID is distinct from the terminal/FIX login;
the CLI records the backend's correct metadata identity pin automatically.

Secret values have no argv options. For automation, `--ctrader-secrets-stdin`
reads one JSON document, up to 64 KiB. Supply it through stdin from a secret
manager or an owner-only file; never put tokens into shell command arguments:

```sh
trade-control-accounts add another-account --broker ctrader --kind demo \
  --ctrader-account-id 123456 --ctrader-secrets-stdin < /private/credentials.json
```

The JSON fields are `client_id`, `client_secret`, `access_token` and optional
`fix`. The `fix` object has `quote` and `trade` objects containing `host`, `port`,
`sender_comp_id`, `target_comp_id`, `sender_sub_id` and `password`, plus a
`broker_name` string. QUOTE/TRADE sub-IDs must match their session names. The
supplied endpoint and account identities are validated together. Every generated
credential/binding file is private (`0600` on Unix); unrelated profiles and TOML
comments are preserved. Malformed credential input errors withhold secret values.

For repeat purchases, verified program policy and cost files make registration
repeatable. Replace the example identity/balance below with actual values:

```sh
trade-control-accounts add my-next-prop --broker ctrader --kind demo \
  --ctrader-account-id 123456 --ctrader-reuse existing-app-profile \
  --prop-firm --initial-balance 10000 --new-flat-account \
  --risk-policy-file /path/verified-program-risk.toml \
  --risk-costs-file /path/verified-instrument-costs.toml \
  --rules-verified --fees-verified
```

The policy file is a standalone `adaptive-risk::RiskPolicy` TOML (the contents of
an account's `.risk` table). The costs file maps native instrument names to
`opening_per_unit`, `closing_per_unit`, `future_swap_per_unit`,
`fixed_per_order` and `overlapping`, in account currency per broker unit.
Alternatively repeat `--risk-cost SYMBOL:OPEN:CLOSE:FUTURE_SWAP[:FIXED]`.
Cost estimates and commission conventions are explicit; zero means verified zero.
The command does not infer a firm's rulebook, estimate fees from the signup price,
or query authenticated broker state during registration.

Individual flags override the policy file. The main ones are `--risk-base-pct`,
`--daily-loss-limit-pct`, `--internal-daily-stop-pct`, `--max-account-loss-pct`,
`--internal-account-stop-pct`, `--daily-reset`, `--risk-timezone`,
`--drawdown-reference`, `--daily-reference`, `--streak-levels`,
`--drawdown-levels`, `--slippage-buffer-pct-of-risk` and `--max-data-age-seconds`.
For example streak tiers are `0:1,1:0.75,2:0.5,3:0.25,4:0.1`; drawdown tiers
are `0:1,2.5:0.5,3.5:0.25,4.5:0.1,5:0`. Disabling a backoff is explicit through
`--no-streak-backoff` or `--no-drawdown-backoff`; the other loss budgets remain.
Without a policy file, firm limits and reference formulas must be supplied
explicitly or entered interactively; the generic 3%/6% example limits are not
silently treated as purchased-program terms.

`--new-flat-account` attests that the account is unused/flat and its firm day-start
references equal its verified initial balance. Otherwise provide
`--day-start-balance` and `--day-start-equity`. `--history-from` overrides the
initial enrollment timestamp and must not be in the future. Initial enrollment
starts with reconciliation required; runtime broker checks must succeed before
any entry. Existing risk state is never reenrolled or reset by this command.

For equity-based daily references or trailing-equity peaks, set
`--firm-evidence-file /absolute/path/to/authoritative-report.json`. The CLI records
the path, not an invented report. An account-specific authoritative producer is
still needed, including coverage of unobserved intervals with open risk. Missing
or stale evidence blocks entries. See the [risk guide](adaptive-risk/README.md).

The CLI writes the named OAuth profile, optional FIX profiles/binding, enabled
risk policy, durable initial state and sticky physical-account activation record,
then registers metadata. File updates are serialized and staged. Risk state is
locked throughout enrollment. Ordinary credential-file/metadata errors roll back
non-prop additions; if a prop metadata insert fails, protection/configuration is
retained because the database outcome could be ambiguous. Inspect `get`/`risk`,
then retry `add` without risk flags to reuse those files/state. Never delete state
or activation records to retry. PostgreSQL and local files are not a distributed
crash-atomic transaction; interrupted registrations fail closed and require
inspection.

List/get output now shows prop risk parameters; `list --json` includes the
non-secret `prop_risk` profile. The local risk command also shows the persisted
streak, locks, day reference and reservation count, without requiring a database:

```sh
trade-control-accounts risk my-prop-account
trade-control-accounts credentials my-prop-account
trade-control-accounts credentials my-prop-account --ctrader-secrets-stdin \
  < /private/replacement-credentials.json
```

Credential rotation preserves metadata caps, risk references, outcomes, streaks
and reservations. An enrolled policy/physical identity cannot be replaced through
credential rotation or another `add --prop-firm`; that requires explicit
migration. Register a different physical account under a new name. Removing a
metadata row does not remove its credentials or sticky risk protection. To add
the same already-configured account to another database, omit risk/credential
flags so its existing profile and risk state are reused.
