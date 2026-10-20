# cTrader Open API integration

## FIX quote-only accounts

An account with cTrader FIX credentials can be registered before Open API OAuth
is available. Store the QUOTE session credentials in owner-only (0600)
`~/.config/trade-control/ctrader-fix-accounts.toml`, or override its location with
`CTRADER_FIX_ACCOUNTS_FILE`. Each named table matches the account index name:

```toml
[ctrader-demo]
host = "your-broker-host"
port = 5211 # The SSL QUOTE port from your own credentials panel.
sender_comp_id = "demo.broker.123456"
target_comp_id = "cServer"
sender_sub_id = "QUOTE"
password = "your-FIX-password"
```

```sh
trade-control-accounts add ctrader-demo --broker ctrader --kind demo
trade-control-accounts list --json
trade-control-broker-check ctrader-demo --instrument AUDUSD
```

Registration validates the named configuration's environment and optional pinned
login. JSON account listings include configured quote/execution/history
capabilities and readiness; they do not claim broker connectivity. The checker
authenticates FIX, discovers the exact symbol and reads real bid/ask prices.
FIX profiles support quotes only: no orders, positions, equity, candle history,
paper plan evaluation or account swaps. Servers reject their execution/history acquisition before opening
a socket. Existing plans are not migrated by account registration.

The worker, cron programs and CLI must all see the same private named profile
file (or identical `CTRADER_FIX_ACCOUNTS_FILE` override). A named Open API profile
takes precedence when present; malformed or inaccessible Open API configuration
is an error and never silently falls back to FIX. The FIX login is not an Open
API `ctidTraderAccountId`; discover the latter independently when enabling OAuth.

cTrader is an account platform shared by many brokers. This integration has two
layers: the standalone `ctrader-api` crate implements the official transport and
OAuth session, while `broker-ctrader` adapts it to trade-control's `Broker` trait.
No TradingView browser session or MT5 bridge is used.

## Account setup

MT5 usernames/passwords do **not** authenticate cTrader Open API. The provider
must supply a cTrader account linked to your cTrader ID. An approved Open API
application and OAuth consent are required; use the `ctrader-api` OAuth CLI to
obtain a token and list the actual `ctidTraderAccountId` values before configuring.

The token copied from cTrader Web's Remote MCP setup is a different credential.
It authenticates the Remote MCP service while the cTrader Web session is active;
it cannot replace the Open API application's OAuth access token in this backend.

Store the app and account credentials in
`~/.config/trade-control/ctrader-accounts.toml` (or `CTRADER_ACCOUNTS_FILE`). The
file must have mode 600. Each section is the exact operator account name:

```toml
[competition-ctrader]
client_id = "your-approved-application-id"
client_secret = "your-application-secret"
access_token = "your-OAuth-access-token"
account_id = 123456
# This means the cTrader API environment, not whether you personally pay for it.
# Get this value from the authorized account discovery response.
environment = "demo"
```

Register the same name in the trade-control account index:

```sh
trade-control-accounts add competition-ctrader --broker ctrader --kind demo
trade-control-broker-check competition-ctrader --instrument EURUSD
```

Run `trade-control-accounts add --help` for required cap/options on your build.
The checker authenticates and reads symbol metadata, equity and a fresh quote.
It does not submit an order. Existing MT5-only candle/preview flags are refused
for cTrader instead of being silently ignored.

Account metadata environment must match the pinned cTrader configuration before
a socket is opened. If the legacy `--oanda-account-id` field is supplied, it is
also checked against the pinned cTrader account ID. Dispatch retains the normal
live-account authorization gate; selecting `ctrader` does not enable live trading.

## Connections

Repeated worker operations reuse one persistent authenticated connection per API
environment. This initial adapter supports **one active account per environment
per process**. Selecting a second account in the same environment fails
explicitly; it does not repeatedly disconnect and reconnect both accounts.
Credential rotation for the same account closes the old connection before a new
one is opened. A dead connection is re-established by the next operation; order
mutations are never automatically replayed after a disconnect.

The quote collector retains symbol subscriptions across operations and requires
both sides of the quote to be fresh. Separate CLI and worker processes have
separate connections; keep diagnostic processes short lived and avoid running
several daemons against the same environment. cTrader recommends at most two
connections (one demo, one live), and multiplexing multiple accounts over one
connection remains a follow-up.

## Current scope

The cron engine and common broker maintenance paths route through the cTrader
adapter, including quotes, candle reads and trade history. Initial sizing is FX
only and uses the selected account's actual contract and currency metadata.
Unsupported assets or incomplete metadata fail explicitly.

Entry execution currently supports stop and limit orders. Market entries,
including market fallback, are refused before submission: relative market
protection would move the strategy's absolute SL/TP when the fill slips. Enabling
them requires verified post-fill correction and recoverable order tracking.

Historical bid/ask updates at one millisecond are applied together. Multiple
distinct prices on the same side at one millisecond cannot be ordered reliably
and are refused, as is incomplete or stalled pagination. No other broker's
prices are substituted.

Matched deal history can confirm a closure without establishing its net P&L.
The adapter retains the raw fee/swap ledger and leaves net P&L unknown until the
broker's fee conventions have been verified; missing values are never zero.

`tv-arm --broker ctrader --account-id competition-ctrader` resolves native
instrument precision from the account. It requires an account-scoped cTrader
spread baseline; no MT5/OANDA/TradeNation spread estimate is borrowed. cTrader
baseline collection/baking is a follow-up. The existing `TV_ARM_ALLOW_UNBAKED=1`
override is explicit and logs that no spread forecast is available.

The replay CLI has no cTrader candle source yet. Chaining a cTrader plan into
replay is refused; replay against another broker would give misleading results.
Instrument and sizing mappings must be audited before moving a plan between
cTrader and another platform. Re-arm explicitly after
checking the new account's native contract metadata.

Do the read-only broker check first. Then use a demo account to validate order
submission, stops, amendments, fills, disconnect recovery and broker history
before enabling unattended management. No authenticated broker session or
demo/live order execution has been validated during implementation.

## Account-scoped CLI controls

For cTrader, `prep`, `veto`, `clear-prep`, `clear-veto` and `unlock` require
`--broker ctrader --account <named-open-api-profile>`. They validate the actual
account and native instrument before signing or posting state changes, and use
the same account scope in the signed intent. FIX QUOTE profiles are refused;
`--force` cannot bypass cTrader account or instrument validation. Other brokers
retain their existing command defaults and `--force` behavior.
