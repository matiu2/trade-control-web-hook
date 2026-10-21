# cTrader broker integration

`CtraderBroker` selects the named account's configured transport. The protocol
clients remain in the separate root `ctrader-api` and `ctrader-fix` crates.

`market_data::ChartData` is a read-only companion view for native BID charts. It
reuses authenticated sessions and explicit Hybrid account binding, and exposes
catalogues, per-reader metadata, quotes and native trendbars with actual tick
volume. Its metadata cache does not feed order sizing. Native candles preserve
their broker opening timestamps and never masquerade as MID/bid-ask execution
history. Requests use Spotware's period-specific history limits and paginate
truncated responses; failed/incomplete pages return an error rather than partial
history. `is_closed()` lets a caller deliberately acquire a new read-only session
on its next request after disconnection. No broker mutations are exposed.

## Capabilities

| Configuration | Quotes | Positions / pending orders | Cancel / close | Protected strategy entries | Candles / equity / deal history |
| --- | --- | --- | --- | --- | --- |
| Open API OAuth | Yes | Yes | Yes | Yes, subject to existing sizing and protection checks | Yes |
| Explicit FIX + Open API binding | FIX | Full positions from Open API; pending orders from FIX | FIX | Open API with native protection | Open API |
| Paired FIX QUOTE + TRADE | Yes | Yes | Yes | Unavailable | Unavailable |
| FIX QUOTE only | Yes, through chart / quote diagnostics | Unavailable | Unavailable | Unavailable | Unavailable |

Open API configuration takes precedence unless an explicit companion binding selects the combined backend. A malformed OAuth profile remains an
error; it does not silently switch to FIX. Paired FIX profiles must identify the
same account and demo environment. The metadata account ID for FIX remains the
terminal login; it is not Open API's `ctidTraderAccountId`. The facade's
`account_id()` returns `None` for FIX rather than mislabel a terminal login.
FIX management currently supports demo accounts only.

Shared profile files, mode 600:

- `~/.config/trade-control/ctrader-accounts.toml`: Open API OAuth.
- `~/.config/trade-control/ctrader-fix-accounts.toml`: FIX QUOTE settings.
- `~/.config/trade-control/ctrader-fix-trade-accounts.toml`: FIX TRADE settings.

Each table name matches the registered trade-control account. FIX QUOTE and
TRADE use their own SSL endpoints. Exact sender and target account identities
are validated before management operations. No broker credentials are logged.

## What risk data means

The strategy's risk sizing needs current account equity, account currency,
symbol base/quote currencies, a current conversion rate, and the broker's
minimum/maximum/step volume. It combines those with the stop distance to compute
units and enforce the configured percentage cap. An explicit unit size still
needs equity and conversion to check that cap. An old equity screenshot is not
a current account-data source, and the demo test's 1,000-unit EURUSD minimum is
not a universal default for other symbols or brokers.

cTrader FIX supplies prices and trading operations, but not balance, leverage,
margin or historical candles. Its documented requests also cannot attach or
amend native stop-loss/take-profit protection. Reports may include native
protection set through another interface; those optional values are retained.
See [FIX limitations](https://help.ctrader.com/fix/limitations/) and
[Spotware's protection explanation](https://community.ctrader.com/forum/fix-api/11964/).

The FIX adapter therefore refuses strategy entries and native stop amendments.
It never drops an entry request's required protection or substitutes guessed
risk inputs. New FIX-only plans are rejected before registration because the
engine requires historical candles for both entries and protection rules.
Protected plan execution requires Open API, or a separately designed data source
and durable synthetic bracket/OCO manager. The raw FIX demo CLI remains useful
for explicitly sized order tests; it is not the protected strategy entry path.

## Management and audit

The normal worker factory, direct CLI controls and broker check can use the
paired FIX adapter. Instrument validation resolves native symbols from this
account's security list without asking for unavailable sizing metadata.
Cancellation resolves the exact broker order and original client ID. Closing
pins the exact position ID and observed quantity; success requires a full fill
and a subsequent position snapshot confirming it is gone. Missing lifecycle
facts remain unknown, and unknown originating order IDs/fill times stay absent.

Operations within a process serialize by full FIX identity. Management writes and combined-mode Open API writes
also take the same account file lock and sync an intent before submitting. Journals
retain intermediate, final and logout reports. The default private audit root is
`$XDG_STATE_HOME/trade-control/ctrader-fix`, or
`~/.local/state/trade-control/ctrader-fix`. `CTRADER_FIX_AUDIT_DIR` overrides it.
Directories must be owned mode 700; journal files must be owned mode 600.

An unresolved submission or failed post-submission verification blocks later
automatic writes, including after restart. Broker reads remain available for
reconciliation. Do not delete or edit journals to make a retry possible. This
initial adapter has no automatic audit-unlock operation; recovery needs explicit
broker reconciliation and an audited operator recovery step.

FIX snapshots alone do not prove a vanished position closed profitably or that
a missing order was cancelled. Full deal history, settlement and cash P/L use
Open API. Retained H status responses may verify known orders, but have no
promised historical retention window.


## Open API companion activation

1. Register and obtain approval for an application in the
   [cTrader Open API portal](https://openapi.ctrader.com/).
2. Use the `ctrader-api` setup CLI to obtain an OAuth grant with **trading**
   permission. Save the named mode-600 Open API profile; FIX passwords and MCP
   tokens cannot authenticate this API. See the root `ctrader-api/README.md`.
3. Inspect the selected account's actual Open API `ctidTraderAccountId`, terminal
   `traderLogin` and `brokerName`. The terminal login must match the FIX account.
4. Create `~/.config/trade-control/ctrader-fix-companions.toml`, mode 600, with an
   explicit named binding (invented IDs below):

```toml
[ctrader-demo]
open_api_account_id = 123456789
fix_sender_comp_id = "demo.example.12345"
broker_name = "Example Broker" # exact brokerName reported by Open API
```

`CTRADER_FIX_COMPANIONS_FILE` overrides this path. The binding pins two different
account identifiers and the broker name; matching numeric terminal logins alone
is insufficient because different brokers may reuse a login. Missing profiles,
wrong environment, mismatched identities and invalid binding files are errors.
The combined connection checks the authenticated trader response before exposing
any broker operations. The registered account's pinned ID stays the FIX terminal
login when this binding is used.

The companion supplies equity, conversion/sizing metadata, historical candles,
protected entries, native stop amendments, lifecycle and settlement. Full position
snapshots also use Open API so stop-loss, originating order and broker fill time
remain available to break-even and blackout protections. FIX supplies quotes,
pending orders, cancellation and closing. The adapter does not merge separately
timed position snapshots or place an unprotected FIX entry while waiting for a
second API to attach its stop.

Configuration advertises capabilities; a successful authenticated broker check
is still required before relying on the companion. Existing FIX-only profiles
continue to expose management only while OAuth approval/setup is pending.


Combined-mode entries and native stop amendments share the FIX account's durable
interlock. A dropped or unknown write on either transport blocks subsequent
automatic writes on both. Protected entries carry the journal correlation ID as
Open API `clientOrderId`; confirmed broker order IDs and requested sizing inputs
are retained. A dry run sends no mutation and remains available for diagnosis.
Structured receipts are emitted at ERROR if journal persistence fails, including
when normal INFO logging is disabled.
