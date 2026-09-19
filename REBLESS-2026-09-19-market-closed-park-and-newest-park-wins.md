# REBLESS 2026-09-19 — market-closed park + "the newest park wins" (32 goldens)

Two causes, both deliberate.

## 1. `StoredReason::MarketClosed` (2 cells, South Africa 40 H1)

South Africa 40 on TradeNation trades 08:00–18:00 Johannesburg (06:00–16:00Z).
The enter fired on the day's LAST H1 bar, which closes at 16:00Z — the instant
the market shuts. It used to be placed at 16:00Z into a closed market (the
market-hours mask only blocks the 15:00Z close hour). It now parks and is placed
at the 06:00Z open. Only `entry_time` moves: `2026-08-12T15:00Z` →
`2026-08-13T06:00Z`. Same fill, same R.

## 2. Replay lookup: the newest PARK wins over an older placement (30 cells)

`ReplayBroker::armed_verified(trade_id)` tried *placed-by-trade-id* before
*parked-by-trade-id*. A trade that had already placed an order and later PARKED a
new fire (below-floor, spread-hour or market-closed) therefore promoted the OLD
placement's intent + shell — a days-stale order at a stale price — and the new
fire was lost. Live is unaffected: it reads the signed intent stored on the park.

| setup | cells | what changed |
|---|---|---|
| aud-cad-h1-2026-07-28 | 8 | entry #1 stops out, re-entry parks below the floor and now promotes ITSELF: 1 leg −1.00R → 2 legs ≈ +0.98R |
| gbp-chf-h1-2026-08-26 | 18 | same shape |
| gbp-zar-h1-2026-07-27 | 4 | second park promotes its own 10:00Z fire instead of re-placing the 07:00Z order |

16 cells gained a leg, 4 lost one, 12 kept their count with different levels.
Every new leg is a re-entry AFTER the previous one closed (checked per setup;
a third fire while a leg is open is still rejected `trade-already-open`).
Unit test: `a_new_park_wins_over_an_older_placement_of_the_same_trade`.

Per-setup, not a sum: the cells of one setup are alternative configs of one trade.
