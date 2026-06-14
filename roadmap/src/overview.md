# Overview

## What this system is

A **synergy between human and machine**: the human does what humans are best at
(reading charts and patterns), the machine does what machines are best at
(following rules without fatigue, at 3am while the human sleeps). The original
motivation was to capture trade entries that happen when London and New York
overlap — a window the operator is asleep for.

## Data flow

```
user annotates TradingView
   -> tv-arm            (reads the chart, controls the arming)
   -> trade-control     (writes signed intent messages, hooks them to TV alerts)
   -> TradingView       (fires alerts)
   -> web-hook          (processes alerts, maintains state in KV, logs)
   -> broker            (TradeNation mostly; OANDA / others possible later)
```

## Where we are (mid-2026)

- Still on the **demo** account, ~11 months into trading.
- Early June has been profitable on demo, but with several deploys per week.
- **Rule to go live:** the system runs for a week with *no code changes* on
  demo and is reasonably profitable.

The tension: there's a long queue of bugs and features, and a strong desire to
go live and start earning. The roadmap below is sequenced to resolve that
tension — to let us go live *confidently* without freezing development.

## The core insight driving the roadmap

The thing blocking confident go-live is **slow feedback**. Today the loop is:

> fix bug → wait a week on demo → hope similar price action appears → re-analyse
> the trades → guess whether it's actually fixed.

The whole foundation phase exists to collapse that loop to **seconds**:

> fix bug → replay the recorded messages against a broker simulator → see the
> behaviour change immediately.

## Sequencing

The roadmap is ordered so that nothing blocks going live except the thing that
*should* — a replay-verified profitable week.

1. **Event schema & correlation ids** — small, design-only, unblocks everything.
2. **Record & replay** + **broker simulator** — the confidence engine.
3. **Go live** on the demo-proven code.
4. **Proxy / fan-out web-hook** — lets staging stop blocking live.
5. **Multi-account routing** and **cancel-on-range-exit** — fall out cheaply
   once the proxy and schema exist.
6. **Journaling automation** — mostly a *consequence* of good recording.

## The one architectural rule that makes it all work

> Every feature's **decision** is a pure function of `(message, KV state)` living
> in `core/`. Only the **KV read/write and broker call** live in the worker.

Keep that line clean and every feature is replay-testable by construction. The
architecture already mostly follows this (`reversal_veto_plan()`,
`seen_decision`, the gate scripts in `core/`). The discipline is to never let a
*decision* leak into the worker's I/O layer.
