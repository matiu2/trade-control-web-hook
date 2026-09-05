# BUG — `--market-entry` / `--limit-entry` give no visible confirmation the broker actually opened the position

**STATUS: FIXED (v135, 2026-08-30)** — both halves. See "Resolution" at the
bottom. The `close-failed` conflation is fixed at the root (`CloseOutcome`);
the placement-time confirmation is fixed in the CLI. The remaining known gap —
no `EntryAttempt` row, so no cron manages the position — is documented as
intended behaviour rather than fixed, in CLAUDE.md.

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

## Suggested fix — SUPERSEDED

Both suggested fixes were taken in v135; see "Resolution" below for what
shipped. The `close-failed` half was fixed at its root (`CloseOutcome`) rather
than at the log line, and the placement-time half in the CLI. Nothing here is
outstanding.

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

## Resolution (v135, 2026-08-30)

**Sub-possibility (2) is now moot and (1) is much louder.** Both suggested fixes
were taken, and the `close-failed` half was fixed at its root rather than at the
log line.

### `close-failed` no longer conflates "nothing to close" with "call errored"

`Broker::close_positions` returned a bare `bool`. It now returns `CloseOutcome`
— `Closed(n)` / `NothingOpen` / `Errored` (`core/src/broker.rs`).

`NothingOpen` is treated as the **success** it is: the instrument is already
flat, which is what the close asked for. `run_close` maps it to
`ActionResult::Ok("closed: nothing-open")`; only `Errored` stays `Failed`.

This turned out to matter more than the log wording. `ActionResult::Failed` is a
`SeenDecision::Skip`, so the old coding meant the intent id was never consumed
and the close **refired** rather than being recorded as fulfilled.

Both brokers now establish "flat" *positively* instead of inferring it from an
error — OANDA reads the position's units (a flat instrument answers with zero
units, not a 404), the TradeNation adapter counts matching open positions before
delegating. The replay broker mirrors all three arms, so replay == live.

The `ClosePositions` veto shares the mapping; its log line now reads
`closed=nothing-open` rather than `closed=failed`.

### The placement-time line no longer overstates what happened

The worker answers a successful dispatch with a flat `ok` (`action_to_parts`) —
the broker order id goes to the persisted request record, not the response body.
So the CLI had no evidence for the word it was printing. It now prints:

```
accepted by worker: trade_id=pos-nzd-cad-...
  confirm the fill with: trade-control-<env> plan timeline pos-nzd-cad-...
```

and a `--broker-dry-run` — which returns the same 2xx `ok`, and so previously
printed a byte-identical line — now says `DRY RUN` plainly.

### What was NOT fixed, and why

The suggested "persisted record keyed by instrument + timestamp" turned out to
already exist in a better form: `build_position_enter` mints a
`pos-<instrument>-<8hex>` `trade_id`, and the `request_records` row carries the
rich `entered: order=<id>` outcome under it. `plan timeline <trade_id>` reads it.
The gap was that nothing ever showed the operator that trade_id — which the new
CLI line does.

Still true, and now documented in CLAUDE.md rather than fixed: a position entry
is `max_retries: Static(0)`, so `record_placement` is never called and **no
`EntryAttempt` row exists**. Every cron keyed off attempts — breakeven watch,
blackout apply/watch, order-control, the pending sweep — therefore ignores the
position. That is the fire-and-forget contract this path advertises, so it is
left as-is; it is worth revisiting only if that contract changes.
