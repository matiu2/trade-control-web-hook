# BUG: break-even watcher arms off pre-fill history and targets the trigger, not the fill

**Status:** OPEN (not fixed as of `2cfeaf62`, 2026-09-10)
**Severity:** HIGH — silently converts winners into ~0R scratches, live only
**First confirmed incident:** NZD_CAD H4 short, 2026-08-10 (OANDA main account, tickets 2320→2326)
**File:** `trade-control-cron/src/breakeven_watch.rs` (both defects)

---

## Summary

An automated NZD_CAD H4 short filled at **0.82043** and was stopped out **6 minutes 2 seconds
later** at 0.82060 for −152.97 AUD. The designed stop was 0.82527 — **456 ticks** away. The
position had been amended to a stop at **0.82046**, i.e. **3 ticks** from the fill, or **1.6% of
the bar's ATR** (~0.00188).

Left alone, the trade hit its take-profit at 0.81531 three days later for **+1.18R**. All eight
entry-rule replay variants agree on +1.18R; the fixture corpus never reproduces this because
**replay does not share either defect** (see "Why fixtures never caught it").

Two independent defects combine. Either alone is a bug; together they guarantee an immediate
scratch on any trade whose instrument has traded below the arming level at any point in the
previous ~83 days.

---

## Defect 1 — `best_close_toward_tp` never filters to post-fill candles

`trade-control-cron/src/breakeven_watch.rs:211-230`

```rust
let since = now - Duration::seconds(snap.granularity.seconds() * BREAKEVEN_LOOKBACK_BARS);
let candles = fetch_candles(broker, instrument, snap.granularity, since, now).await?;
let bar = Duration::seconds(snap.granularity.seconds());
candles
    .into_iter()
    .filter(|c| c.time + bar <= now)      // <-- ONLY "is it closed". No `>= fill time`.
    .map(|c| c.c)
    .reduce(...)
```

`BREAKEVEN_LOOKBACK_BARS = 500` (`:58`). For H4 that is **500 × 4h ≈ 83 days** of candles.

The only filter is "has this bar closed". There is **no lower bound at the fill time**, despite
the function's own doc comment at `:207-210` stating *"Fetch the closed candles **since the
fill**"* — and the same claim repeated at `:23-25` and `:151`. `EntryAttempt` carries `placed_at`
(`core/src/state.rs:201-230`) and it is never consulted here.

Consequence: **any** bar in the preceding ~83 days that closed past the 50%-to-TP level arms
break-even on the **first cron tick after the fill**, before the trade has moved at all.

### Measured on the actual incident

Arming level for this trade (short): `entry + 0.5 × (tp − entry)` = **0.81788**.

Pulled the 500-bar H4 lookback that the watcher would have fetched
(2026-05-19 → 2026-08-10, 359 bars returned):

| | |
|---|---|
| Bars in window | 359 |
| Bars closing ≤ 0.81788 (i.e. arming BE) | **255 (71%)** |
| Most recent arming bar | **2026-07-30 11:00** — eleven days *before* the fill |
| Lowest close in window | 0.79976 (2026-06-29) |

Break-even was armed by July price action. The trade filled on 2026-08-10 and was scratched by
history it had no relationship to.

---

## Defect 2 — the break-even target is the resolved trigger, not the broker fill

`core/src/state.rs:190-192`:

```rust
/// Resolved entry/reference price at placement — the break-even target stop
/// (a 0R scratch) and the lower bound of the 50%-to-TP window.
pub entry_price: f64,
```

`Breakeven::target_stop(entry) -> entry` (`core/src/intent/breakeven.rs:100`) returns that value
verbatim as the new stop.

`entry_price` is snapshotted **once at placement** from `resolved.entry.reference_price()`
(`core/src/dispatch/enter.rs:951`) — the stop/limit trigger, or for a market entry the signal
bar's close (`core/src/intent/resolution.rs:287-291`). It is **never reconciled to the actual
fill**; `EntryAttempt` has no fill-price field.

### Measured on the actual incident

| | Price |
|---|---|
| Stop-order trigger (snapshot `entry_price`) | 0.82046 |
| **Actual broker fill** (tx 2321) | **0.82043** |
| **Stop the watcher amended to** (tx 2325) | **0.82046** |

The amended stop equals the snapshot trigger **exactly**. For a short, a stop 3 ticks *above* the
fill is not a scratch — it is a **guaranteed 3-tick loss plus slippage**, and it sits far inside
normal noise. OANDA's own trade analysis reports `risk_reward_ratio: 170.67` and
`stop_loss_distance: 0.00003` for the resulting position.

Exit was a further **1.4 pips unfavourable slippage** on the stop (0.82060 vs 0.82046).

Note this defect is *direction-dependent*: for a short filled better than trigger (fill below
trigger) the BE stop lands above the fill → loss. Long trades filled worse than trigger get the
mirror-image problem. Only an exact-trigger fill is harmless.

---

## Broker evidence (OANDA v20, account `m-and-w` main)

```
08-10 23:00:03 Bne  2320  STOP_ORDER      NZD_CAD  -876,934   CLIENT_ORDER
08-10 23:46:00 Bne  2321  ORDER_FILL      @ 0.82043            STOP_ORDER
08-10 23:46:00 Bne  2322  TAKE_PROFIT_ORDER  @ 0.81531         ON_FILL
08-10 23:46:00 Bne  2323  STOP_LOSS_ORDER    @ 0.82527         ON_FILL
08-10 23:52:02 Bne  2324  ORDER_CANCEL    orderID 2323   CLIENT_REQUEST_REPLACED
08-10 23:52:02 Bne  2325  STOP_LOSS_ORDER    @ 0.82046         REPLACEMENT      <-- 3 ticks
08-10 23:52:02 Bne  2326  ORDER_FILL      @ 0.82060  −152.97   STOP_LOSS_ORDER
08-10 23:52:02 Bne  2327  ORDER_CANCEL    orderID 2322   LINKED_TRADE_CLOSED
```

`2324`, `2325`, `2326` share `batchID 2324` and an identical timestamp — the replacement stop was
filled in the same batch that created it. Duration 6m 2s, `realized_pl` −152.972 AUD.

---

## Why fixtures never caught it

Replay implements break-even **correctly** and therefore diverges from live on both counts —
`cli/src/bin/replay_candles/fill_sim.rs:786`:

- it walks only `fill.rest`, i.e. strictly **post-fill** bars (no Defect 1), and
- it arms and targets off `fill.entry_price`, the **actual fill** (no Defect 2).

So the entire fixture corpus is blind to this class of bug. The 8-variant matrix for this very
setup returns +1.18R unanimously, including the `normal` variant that was traded live.

**This means replay-vs-live parity on break-even is untested.** Any live trade whose journalled R
disagrees with its replay R should be checked against this signature before the setup is blamed.

---

## Suggested fixes

1. **Bound the candle window at the fill.** In `best_close_toward_tp`, add
   `.filter(|c| c.time >= fill_time)` using `EntryAttempt.placed_at` (or better, a real fill
   timestamp — see 3). The 500-bar lookback then only bounds the *fetch*, not the *logic*.
   This alone stops the incident.
2. **Reject a BE amend that lands inside noise.** Guard the amend: refuse when
   `|target − current_price|` is below some floor (e.g. `max(spread × k, ATR × 0.1)`), and log
   loudly rather than silently amending. This is defence-in-depth — a correct BE stop should
   never be 1.6% of ATR from market.
3. **Carry the broker fill price back onto the snapshot.** `de91749a` already carries size and
   rate back onto the trade log; extend it to write the fill price into `BreakevenSnapshot
   .entry_price` (or add a `fill_price` field and prefer it). Fixes Defect 2 and makes the 0R
   scratch an actual 0R.
4. **Add a replay-vs-live parity fixture for break-even** so this class can't recur silently.

## Suggested confirmation from logs

`breakeven_watch.rs:175-185` prints an `INTENT amend_stop` line carrying `entry=`, `tp=` and
`best_close=`. For this trade, expect `entry=0.82046` (proving Defect 2) and a `best_close` dated
on or before **2026-07-30** (proving Defect 1). If both hold, this report is fully confirmed from
the system's own logs.

## Related

- `books/demo-journal/src/trade-155-nzdcad-h4-hs-live-scratch-asdesigned-win-1p18r.md` — the journal page
- `books/demo-journal/src/oanda-main-account-aug-2026.md` — Findings 3 / 3b, two further legs
  (AU200 −1,890 closed 82 min after fill; NZD/JPY −490 closed 72 s after fill) with the same
  "correct entry, premature exit" shape but no transaction trail pulled yet. **Both are worth
  re-checking against this signature.**
- Trade 135 (CAD/SGD) — SL→BE 142 s after fill with the threshold unmet; likely the same root cause.
