# Cancel order on range-exit

**Status:** not started · **Phase:** 4 · **Size:** small

Symmetric with the existing too-high / too-low alerts, but for **pending
orders** instead of fills: if price leaves the order's valid range *before* it
fills, cancel the order.

## The trigger

Example: a long stop-entry placed off a bullish pinbar. If price then drops
*below* the pinbar, the pinbar thesis is dead — the pending order should be
cancelled rather than left to fill on a stale signal.

## Shape

- A **Pine-side alert**, like too-high / too-low, but it maps to a *conditional
  cancel* action rather than a veto/reject.
- It targets a *pending* order (by `trade_id` / `intent_id`), not an open
  position.

## Fold into the existing cancel pathway

There's already an **order-expiry + recovery** design in flight (bar-based order
expiry, `cancel_at`, signed `expiry_bars`). Cancel-on-range-exit is another
*reason* to hit that same cancel path — build it **into** that pathway, not as a
parallel one. Both answer "cancel this pending order because a condition fired."

## Acceptance

- [ ] Pine alert fires when price exits the order's range pre-fill.
- [ ] Maps to a conditional-cancel action on the pending order.
- [ ] Reuses the order-expiry cancel pathway (no parallel mechanism).
- [ ] No-op (logged) if the order already filled or was already cancelled.
- [ ] Replay-tested via a recorded "placed then range-exited" scenario.

## Open questions

- Is the "range" the same geometry the entry was armed from (so tv-arm can
  derive it), or a separately specified band?
- Interaction with multi-shot re-entry: does a range-exit cancel kill the whole
  setup or just the current pending order?
