# cTrader Open API integration

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
Account swaps between cTrader and another platform are also refused until their
instrument and sizing mappings have been audited. Re-arm explicitly after
checking the new account's native contract metadata.

Do the read-only broker check first. Then use a demo account to validate order
submission, stops, amendments, fills, disconnect recovery and broker history
before enabling unattended management. No authenticated broker session or
demo/live order execution has been validated during implementation.
