# Live OANDA candle spreads

The `oanda-live` account needs its own historical books and spread calibration.
Changing the API hostname alone is insufficient: OANDA's generic
`/v3/instruments/{instrument}/candles` endpoint returns a base pricing group.
The account's own historical spreads come from
`/v3/accounts/{accountID}/instruments/{instrument}/candles`.

The comparison in
`spread-baseline-gen/validation/2026-10-27-oanda-live-vs-demo.json` matches
complete M1 candles from 14 January, 15 July, and 26 October 2026 across eight
instruments. All four bid/ask OHLC differences are compared with exact decimal
arithmetic at identical timestamps. Live and demo spreads differ, so demo
calibration is retained separately rather than copied to the live account.

Recent-day close-spread medians in OANDA's broker pip units:

| Instrument | Live account | Demo account |
| --- | ---: | ---: |
| EUR_USD | 0.8 | 1.0 |
| GBP_JPY | 3.1 | 3.3 |
| XAU_USD | 33.0 | 38.0 |
| BCO_USD | 3.1 | 4.1 |

Runtime profiles use the existing `instrument-lookup` pip convention so entry
and stop calculations remain in the same units as the instrument definition.
The complete `instrument_lookup::oanda_live` account catalogue now supplies those
units, including the 33 instruments missing from the older native catalogue.
Their profiles have been regenerated from the same account-scoped history so
spread thresholds and live sizing stay aligned. The catalogue also records
broker quantity precision, minimum sizes and observed H1 session masks.
OANDA describes pricing-group differences in its
[historical data guidance](https://help.oanda.com/au/en/faqs/rest-v20-api-troubleshooting-guide.htm).

`OandaClient::with_candle_account` pins every candle method to the account
endpoint. Live `OandaBroker` instances use this mode and a stable hashed account
cache namespace. They do not reuse the earlier generic live cache, and demo
history remains in its existing namespace. The calibration generator and the
live broker share this account namespace.

To regenerate with the live token in `OANDA_TOKEN` (or `OANDA_API_KEY`):

```sh
cargo run --release -p spread-baseline-gen --bin generate-oanda -- \
  --account oanda-live --account-id "$OANDA_ACCOUNT_ID" \
  --report .local-calibration/oanda-live-90d-report.json \
  --out ../candle-cache/src/spread_profiles/oanda.rs
```

The report checkpoints each completed instrument. Rerunning resumes its fixed
UTC window and account identity; changed accounts or catalogues are refused.
This private checkpoint is ignored by Git. Published validation evidence uses
an account fingerprint and deliberately cannot be used as a resume checkpoint.
The output is promoted only after the complete broker account catalogue has
usable measured profiles over a requested window of at least 90 closed calendar
days. Each instrument must also have observations within seven days of both
window edges and at least 80% of the usual weekday count (52 distinct UTC dates
for a 90-day run). Truncated and unreviewed responses remain retryable failures.
A partial `--only` probe cannot replace the runtime table.

The generated keys are `oanda:oanda-live:<native symbol>`. Shared runtime
coverage, spread thresholds, hourly forecasts, and spread-hour widening read
those keys for the live account. Missing instruments in a calibrated account
remain account-qualified and cannot borrow the generic demo profile. Raw
historical bid/ask candles retain their measured spreads; the baked table is a
forecast and normal-spread estimate, not a replacement for historical quotes.

OANDA execution is enabled for live account metadata. `dry_run` defaults to
false; it is enabled only by an explicit dry-run intent. For every entry and
stop amendment the adapter reads current instrument rules from the selected
account. Quantities are floored to `tradeUnitsPrecision`, checked against
`minimumTradeSize` and `maximumOrderUnits`, and submitted as decimal strings.
Market, stop and limit orders retain their attached stop-loss and take-profit.
Prices use the broker's `displayPrecision`; they are not padded to five places
for instruments that reject that precision.

Live replay infers `oanda-live` from the plan's OANDA rules, or accepts
`--oanda-account oanda-live`. Its initial history and sub-bar zoom share the same
account-pinned cache as the live chart. Fixture metadata preserves that account;
mixed demo/live plans and conflicting fixture overrides are refused. Live replay
position drawings retain `broker=oanda-live`. Broker metadata and the live token
are loaded through the same native profile loader used by Local Chart.
