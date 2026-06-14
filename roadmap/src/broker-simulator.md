# Broker simulator

**Status:** not started · **Phase:** 2 (foundation) · **Size:** medium (mostly reuse)

The other half of [record & replay](./record-replay.md): something to replay
broker calls *against* without touching a real account.

## The seam

We already have a broker abstraction (`broker-trait`, `broker-oanda/`, the
tradenation crate). That trait **is** the swap point. The replay harness injects
the simulator where the worker injects the real client.

## What it must do (v1 — keep it dumb)

The simulator does not need to be clever at first:

1. Accept orders; hold pending stop/limit entries.
2. **Fill** a pending order when a recorded candle crosses its level.
3. Apply SL / TP exits.
4. Return fills / order ids / errors in the same shape the real broker does, so
   `BrokerResponse` events match.

Most of this already exists in `trade-simulator` (parent repo). The work is
adapting it to the broker trait and to bid/ask-aware fills (long entry on
`ask`, long exit on `bid`, per the parent CLAUDE.md pricing table).

## Error injection

To replay *broker-error* scenarios (the 502-on-place path, rejections), the
simulator needs a way to return recorded errors at the right point — feed it the
recorded `BrokerResponse` so a replay of a failed placement reproduces the
failure. This is what lets us regression-test the "failed placement does NOT
mark seen" rule (the 2026-06 CHF/JPY incident).

## Acceptance

- [ ] Simulator implements the broker trait used by the worker/CLI.
- [ ] Pending orders fill on recorded candle crossings; SL/TP applied.
- [ ] Bid/ask-correct fill prices.
- [ ] Can replay a recorded broker error (502 path) faithfully.
- [ ] Used by `trade-control replay` in place of the live client.

## Open questions

- Where do the candles for fill simulation come from during replay — recorded
  alongside the alert, or fetched from candle-cache by `(instrument, ts)`?
- Does it need to model partial fills / slippage, or is exact-level fill enough
  for v1? (Start exact; add slippage only if a real discrepancy shows up.)
