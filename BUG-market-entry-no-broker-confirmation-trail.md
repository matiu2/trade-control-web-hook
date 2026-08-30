# BUG (suspected) — `--market-entry` / `--limit-entry` give no visible confirmation the broker actually opened the position

**Found:** 2026-08-30, while journaling a NZD/CAD H&S short (demo-journal trade 152).

## Symptom

On 2026-08-14 14:53 the operator registered a pattern plan
(`ihs-nzd-cad-37926360`, iH&S long, account `reversals`) and, believing the
same session, also ran `tv-arm-staging --market-entry register` reading a
drawn position tool on the chart, intending to enter manually.

The engine's own `05-enter` fired 2026-08-15 01:00 and was rejected
(`sl-widen-below-min-r`), so no engine-placed order exists. The operator
*believed* the manual `--market-entry` order had gone through and was being
managed by the system. Nine days later, `07-close-on-sr-reversal` fired
(2026-08-18 13:00) and the dispatch log recorded:

```
dispatch: close-failed
```

The operator read this as "the system tried to close my position and
failed" and took no further action — leaving what they believed was an
open, unmanaged short for over a week.

## What the evidence actually shows

Pulling the TradeNation account's raw activity log
(`tradenation activity list`) for this instrument across the entire period:

- **Zero `Open Position` / `Execute Order` events for NZD/CAD between
  2026-08-05 and 2026-08-27.**
- The next NZD/CAD activity at all is 2026-08-27 20:07:21 (`Open
  Position:27321014`) — a *different*, later, unrelated manual entry
  (channel `Client`, i.e. placed through the normal order-entry path, not
  traceable to the 08-14 `--market-entry` invocation).

So `close-failed` on 08-18 was **correct**: `close_positions()`
(`broker-tradenation/src/orders.rs:248`) filters the broker's live
positions by instrument name and returns `closed_count > 0`; with no open
NZD/CAD position at that time, it correctly found nothing to close and
reported failure. The close dispatch is not the bug.

The actual gap: **the 08-14 `--market-entry` invocation never resulted in
an open broker position, and nothing told the operator that.** Two
sub-possibilities, not yet distinguished:

1. The CLI's early-return guard (`position_entry.rs:51-61`) — `if
   roles.position.as_ref()` is `None`, it `eprintln!`s an error and returns
   exit code `1` — fired because no position tool was drawn/read at that
   moment, and the printed error was missed in a multi-trade session.
2. The signed enter intent was built and POSTed
   (`register_post::post_intent_blocking`), but the **worker-side
   placement failed** (bad spread, broker rejection, session issue) and
   that failure wasn't surfaced back to the operator's terminal in a way
   that was noticed.

Either way, there is no artifact — no follow-up log line, no `plan show`
entry (this path has no `trade_id`/plan at all, per `position_entry.rs`'s
own doc comment: "no plan, no engine rules, no preps or vetos"), nothing —
that would let an operator later confirm "did my `--market-entry` actually
fill?" without manually cross-referencing the raw broker activity/
transaction export.

## Why this matters

`--market-entry` / `--limit-entry` are explicitly the **no-plan, no-engine,
fire-and-forget** path (per the doc comment in `position_entry.rs`): once
placed, `07-close-on-sr-reversal` and friends are the *only* automated
touchpoint the operator has for that position (if they also registered a
plan alongside it). If the initial placement silently fails, and the only
future signal is a terse `close-failed` on an unrelated later fire, an
operator has no reliable way to notice the discrepancy except by manually
pulling broker records — which is what happened here, nine days late.

## Suggested fix (not yet scoped)

- Make `run_position_entry`'s failure path louder / harder to miss — e.g.
  a non-zero-but-distinct exit code, and/or a persisted record (even a
  simple KV row keyed by instrument + timestamp) so a later `plan show`-
  style command could answer "was there a `--market-entry` fill here."
- Consider having `close-failed` distinguish "no matching position found on
  broker" (arguably not a failure at all — it's a no-op) from "broker
  call errored" (a real failure), so `close-on-sr-reversal` /
  `close-on-reversal` logs read as an intelligible signal instead of a
  generic `close-failed` either way. Right now both cases collapse to the
  same string (`core/src/dispatch/close.rs:251-255`), which is exactly
  what led the operator to misread "nothing to close" as "tried and
  failed."

## Evidence

- Plan: `ihs-nzd-cad-37926360` (`plan show` / `plan timeline`, account
  `reversals`)
- TradeNation activity export: `books/demo-journal/reversals.activity.json`
  (2026-08-30 pull) — filtered `market == "NZD/CAD"`, no rows between
  2026-06-12 and 2026-08-27
- `close_positions`: `broker-tradenation-adapter/src/lib.rs:67-69` →
  `broker-tradenation/src/orders.rs:248-270`
- `run_close`: `core/src/dispatch/close.rs:236-255`
- `--market-entry` CLI path: `tv-arm/src/position_entry.rs:40-61`
