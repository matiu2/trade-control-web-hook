# Bug: `too-low` / pcl-exhausted veto closes open positions (should be entry-blocking only)

**Severity:** High — silently closes winning trades before TP.
**Component:** `cli/src/trade_patterns.rs` (alert template generation).
**Found via:** Demo trade 046, CHF/JPY H1 H&S short, 2026-06-03. Logged in
`books/demo-journal/src/trade-046-chfjpy-hs-toolow-close.md`.

**Status: FIXED** — see commit landing the `VetoLevel` parameter on
`build_invalidation_alert`. The pcl-exhausted veto now emits
`level: stop-next-entry`; the invalidation veto stays `close-positions`.
Regression tests: `build_trade_from_spec_pcl_exhausted_veto_shares_shape_but_not_level`
and `pcl_exhausted_veto_never_closes_positions_for_both_patterns` in
`cli/src/trade_patterns.rs`.

## Summary

The `too-low` veto (for shorts) closed a live, in-profit short ~31 ticks
before its take-profit. `too-low` is the **pcl-exhausted** veto — its job is
*"price has already run most of the way to TP, so don't open a late entry."*
It is purely an **entry-blocking** condition. But it was emitted with
`level: ClosePositions`, so when it fired while a position was already open, it
**flat-closed that position** — even when price was moving in the trade's favour.

For a short, "too low" means *price has dropped* — which is **profit**, the
exact opposite of a reason to exit. The veto fired on a strong down-candle
(the move we wanted) and closed the winner early.

## Evidence (trade 046)

Verified by reconciling the TradingView alert CSV with the Cloudflare Worker
server-side JSON:

```
2026-06-03T07:51:39Z  POST  hs-chf-jpy-efd5e647-veto-too-low  (bar 07:00, level=ClosePositions)
2026-06-03T07:51:43Z  INFO broker_tradenation::orders: closed market=CHF/JPY position_id=27169081
2026-06-03T07:51:43Z  veto set: name=too-low level=ClosePositions closed_ok=true
```

- The intended exit path (`close-on-reversal`, golden-candle/news-window) was
  **rejected every time** it fired (`no-news-window | price-out-of-range`), so
  it was *not* responsible.
- The close happened **8 minutes before** the BOJ Ueda news window opened, so
  news is not the cause.
- Result: booked **+3.76R** instead of the **+5.05R** the trade was tracking
  (broker order: entry 203.259, SL 203.502, TP 202.033 — 24.3-tick risk,
  122.6-tick TP). The veto closed after 91 of 122.6 ticks (**74.5% of the way to
  TP**), costing **~1.29R / ~£812**. On a losing day, the same misfire could
  convert a win or scratch into a loss, purely on a static price floor regardless
  of direction.

## Root cause (exact location)

`build_invalidation_alert()` hard-coded the level for **both** vetos it builds:

```rust
intent.level = Some(VetoLevel::ClosePositions);
```

That builder is reused for two semantically different vetos:

```rust
build_invalidation_alert(.., geometry.invalidation_veto_name,   ..)  // too-high (short) — correct: close on structure break
build_invalidation_alert(.., geometry.pcl_exhausted_veto_name,  ..)  // too-low  (short) — WRONG: should be entry-block only
```

The two vetos are **not** symmetric in meaning:

| Veto (short) | Means | Correct action |
|---|---|---|
| `too-high` (invalidation) | price ran back up past the right shoulder → setup is **dead / invalidated** | `ClosePositions` ✅ (close — the thesis is broken) |
| `too-low` (pcl-exhausted) | price already ran most of the way **to TP** without us in | `StopNextEntry` ✅ now (only block a *new* late entry; never touch an open winner) |

`VetoLevel` semantics (`core/src/intent.rs`) are correct and were not the
bug — the bug was that the pcl-exhausted veto was **assigned the wrong level**
at template-generation time.

## Fix

1. `build_invalidation_alert()` now takes a `level: VetoLevel` parameter.
   - Invalidation veto call site → `VetoLevel::ClosePositions` (unchanged).
   - Pcl-exhausted veto call site → `VetoLevel::StopNextEntry` (the fix).
2. The misleading `PatternGeometry::pcl_exhausted_veto_name` doc-comment and
   the builder's `purpose` string were corrected.
3. Regression tests added (see Status note above).

## Audit of other `ClosePositions` vetos

- `too-high` (invalidation) — fires when price runs back past the right
  shoulder, i.e. *against* the trade, structure broken. Genuine thesis
  invalidation. **Correct, kept at `ClosePositions`.**
- `trade-expiry` — fires at wall-clock expiry (`not_before = trade_expiry`),
  meaning "the setup's planned window is over". Not a price-relative trigger,
  so it can't spuriously fire in the trade's favour. Flattening a stale trade
  past its window is the intended belt-and-braces. **Correct, kept as-is.**

## Defensive runtime guard (optional, not implemented here)

A belt-and-braces follow-up: before any veto-driven `ClosePositions` executes,
refuse to close a position whose unrealised P&L is positive and whose
triggering price moved in the position's favour — log and ignore instead. This
would catch a future mis-tag regardless of the template-time level. Not built
in this fix; the level correction above is the primary remedy.
