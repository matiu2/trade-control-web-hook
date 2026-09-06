# Broker evidence — EUR/CAD 2026-08-07/08 (plan `hs-eur-cad-b6b71f6c`)

Companion data for `BUG-cancelled-limit-order-resolves-unknown-blocks-reentry.md`.
Everything below is pulled verbatim from OANDA, not from our system's logs.

- **Account:** `101-011-31142393-003` (OANDA **practice**, alias `m-and-w`), AUD
- **Balance now:** 390,435.4865 · **open trades: 0** · `lastTransactionID`: **2453**
- **Instrument:** EUR_CAD · **Plan:** `hs-eur-cad-b6b71f6c`, H1, armed
  `--strategy-v2 --qm-entry=market`
- **Pulled:** 2026-09-06 via `oanda-mcp` `get_transaction` / `list_transactions`
  (the tools added in `FEATURE-transaction-lookup.md`)

---

## 1. The two orders our system reported as `entered:`

Our plan log claimed:

```
2026-08-08 00:59 +10:00 • fired 09-enter-qm (enter) → entered: order=2316
2026-08-08 02:00 +10:00 • fired 09-enter-qm (enter) → entered: order=2318
```

The broker's record of those same two ids:

```json
{
  "id": "2316", "transaction_type": "LIMIT_ORDER",
  "time": "2026-08-07T15:00:11.283218193Z",
  "instrument": "EUR_CAD", "reason": "CLIENT_ORDER",
  "opened_trade_id": null, "is_failure": false,
  "raw": { "batchID": "2316", "units": "-865150", "type": "LIMIT_ORDER" }
}
{
  "id": "2318", "transaction_type": "LIMIT_ORDER",
  "time": "2026-08-07T16:00:41.406477180Z",
  "instrument": "EUR_CAD", "reason": "CLIENT_ORDER",
  "opened_trade_id": null, "is_failure": false,
  "raw": { "batchID": "2318", "units": "-672809", "type": "LIMIT_ORDER" }
}
```

**Both accepted cleanly** — `is_failure: false`, no `rejectReason`, `opened_trade_id:
null`. They were **LIMIT** orders, resting, unfilled.

## 2. What actually happened to them — the decisive rows

```
2316  2026-08-07T15:00:11Z  LIMIT_ORDER   EUR_CAD  units=-865150   CLIENT_ORDER    ok
2317  2026-08-07T16:00:38Z  ORDER_CANCEL  orderID=2316             CLIENT_REQUEST
2318  2026-08-07T16:00:41Z  LIMIT_ORDER   EUR_CAD  units=-672809   CLIENT_ORDER    ok
2319  2026-08-07T17:00:23Z  ORDER_CANCEL  orderID=2318             CLIENT_REQUEST
```

Full JSON for the two cancels:

```json
{
  "id": "2317", "transaction_type": "ORDER_CANCEL",
  "time": "2026-08-07T16:00:38.108475384Z",
  "reason": "CLIENT_REQUEST", "is_failure": true,
  "raw": { "batchID": "2317", "orderID": "2316",
           "reason": "CLIENT_REQUEST", "type": "ORDER_CANCEL" }
}
{
  "id": "2319", "transaction_type": "ORDER_CANCEL",
  "time": "2026-08-07T17:00:23.864967250Z",
  "reason": "CLIENT_REQUEST", "is_failure": true,
  "raw": { "batchID": "2319", "orderID": "2318",
           "reason": "CLIENT_REQUEST", "type": "ORDER_CANCEL" }
}
```

**`CLIENT_REQUEST` = our own API call cancelled them.** Not the broker, not an
expiry, not a rejection.

Neither order ever filled. There is **no `ORDER_FILL` referencing orderID 2316 or
2318** anywhere in the transaction stream, and:

```
get_trade_analysis("2316") -> 404 NO_SUCH_TRADE   (lastTransactionID: 2453)
get_trade_analysis("2318") -> 404 NO_SUCH_TRADE
```

## 3. Timing — cancels land on the H1 bar boundary

| Order | Placed (UTC) | Cancelled (UTC) | Lifetime |
|---|---|---|---|
| 2316 | 15:00:11 | 16:00:38 | 60m 27s |
| 2318 | 16:00:41 | 17:00:23 | 59m 42s |

Order 2318 was placed **3 seconds after** 2316 was cancelled — a cancel/replace
pair. Then after 2319 cancelled the second order at 17:00:23Z, **no third order was
ever placed**. The re-price loop stopped.

Both timestamps are **Friday, mid-New-York session** (15:00Z = 11:00 NY, 16:00Z =
12:00 NY). Peak EUR/CAD liquidity — market-closed / illiquidity is ruled out.

## 4. Proof the account was healthy either side

Immediately after, on the same account and credentials, a normal entry works
end-to-end:

```
2320  2026-08-10T13:00:03Z  STOP_ORDER  NZD_CAD  units=-876934  CLIENT_ORDER
2321  2026-08-10T13:46:00Z  ORDER_FILL  NZD_CAD  price=0.82043  reason=STOP_ORDER
                            tradeOpened: { tradeID: "2321", units: "-876934" }
2322  2026-08-10T13:46:00Z  TAKE_PROFIT_ORDER  ON_FILL
2323  2026-08-10T13:46:00Z  STOP_LOSS_ORDER    ON_FILL
```

That is what a working entry looks like: order → fill → `tradeOpened` → brackets
attached. Eleven trades filled on this account between 08-10 and 08-14 (XCU_USD,
AU200_AUD, FR40_EUR, NZD_CAD, BCO_USD), including on 08-14 itself.

**So: not credentials, not connectivity, not margin, not account state.**

## 5. Trade-id sequence shows the gap

EUR_CAD trades on this account, all time: **9** — the most recent is **2026-07-22**
(trade 2241, the unrelated manual trade-133 entry). **Zero EUR_CAD trades between
2026-08-07 and 2026-08-18.**

Trade ids around the window:

```
2271  2026-07-23  GBP_NZD
2292  2026-07-24  NZD_JPY
2303  2026-07-25  NZD_JPY     <- last trade before
  [2316, 2318 = EUR_CAD orders, NEVER became trades]
2321  2026-08-10  NZD_CAD     <- next trade after
```

The ids were issued and consumed (lastTransactionID 2453 is far beyond them), they
sit correctly in chronological order, and they produced no trades.

## 6. Bonus finding — `ORDER_CANCEL_REJECT: ORDER_DOESNT_EXIST` storms

Not the cause of this bug, but visible in the same range and probably worth a
separate look. Our system repeatedly tries to cancel orders that no longer exist,
in bursts of 2-3 one second apart:

```
2300  2026-07-24T15:51:44Z  ORDER_CANCEL_REJECT  orderID=2291  ORDER_DOESNT_EXIST
2301  2026-07-24T15:51:45Z  ORDER_CANCEL_REJECT  orderID=2291  ORDER_DOESNT_EXIST
2310  2026-07-26T22:50:07Z  ORDER_CANCEL_REJECT  orderID=2270  ORDER_DOESNT_EXIST
2311  2026-07-26T22:50:08Z  ORDER_CANCEL_REJECT  orderID=2270  ORDER_DOESNT_EXIST
2312  2026-07-26T22:50:09Z  ORDER_CANCEL_REJECT  orderID=2270  ORDER_DOESNT_EXIST
2313  2026-07-27T00:45:56Z  ORDER_CANCEL_REJECT  orderID=2302  ORDER_DOESNT_EXIST
2314  2026-07-27T00:45:58Z  ORDER_CANCEL_REJECT  orderID=2302  ORDER_DOESNT_EXIST
2315  2026-07-27T00:45:59Z  ORDER_CANCEL_REJECT  orderID=2302  ORDER_DOESNT_EXIST
```

Of 17 transactions in the range 2300-2316, **10 are failures**. This is the same
class of problem as the main bug — our side holds a stale belief about an order's
existence — and may share a root cause with it. Note 2302 is an order that *did*
fill (as trade 2303) and was later cancel-attempted three times anyway.

## 7. What our system believed, for contrast

```
2026-08-08 00:59  09-enter-qm  → entered: order=2316          (actually: resting limit)
2026-08-08 02:00  09-enter-qm  → entered: order=2318          (actually: resting limit)
2026-08-08 04:59  07-close-on-sr-reversal → close-failed      (nothing to close)
2026-08-08 06:00 .. 2026-08-11 15:00
      05-enter    x8  → rejected: prior-attempt-unknown
      09-enter-qm x4  → rejected: prior-attempt-unknown
2026-08-17 23:00  06-close-on-reversal    → close-failed      (nothing to close)
2026-08-19 00:00  02-veto-trade-expiry    → cancelled=0 closed=failed
```

Replay of the same plan over the same window returns **+2.63R** (one reversal-close
scratch +0.08R, one TP +2.55R). Live returned **0R**. The journal recorded a WIN.

## 8. Reproducing the pull

```
oanda-mcp (account 101-011-31142393-003, OANDA practice):
  get_transaction(transaction_id="2316")
  get_transaction(transaction_id="2318")
  list_transactions(from_id="2316", to_id="2325")
  list_transactions(from_id="2300", to_id="2316")
  list_trades(state="all", instrument="EUR_CAD", count=100)
  get_trade_analysis(trade_id="2241")   # control: a real trade, returns full detail
```

Fixture for the replay side:
`replay-fixtures/eur-cad-h1-2026-08-07-strategy-v2-qm-market-news-off`
(expected +2.63R; 8 variants exist under `eur-cad-h1-2026-08-07-*`).
