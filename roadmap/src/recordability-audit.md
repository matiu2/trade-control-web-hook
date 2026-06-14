# Recordability audit — would the schema have caught it?

Validating the [event schema](./event-schema.md) against **real, documented
incidents**. For each: what diagnosis cost the operator, and whether the
Phase-0 debugging-first schema would have made it a log-grep instead. This is the
acid test — a schema that doesn't shorten *past* investigations won't shorten
future ones.

Verdict key: ✅ caught cleanly · ⚠️ partially · ❌ still hard.

---

## 1. CHF/JPY mark-seen poisoning (2026-06-02) — ✅

**What happened.** An `enter` alert fired 6× in 9h. Fire 4 was correctly rejected
(`missing-prep`), but the old "mark seen on every outcome" rule poisoned the
intent id, so fires 5 and 6 (the entry the operator wanted) 409'd before reaching
the gate. Operator entered manually.

**Diagnosis cost then.** Cross-referencing 6 fires across TV CSV + CF logs to work
out *which* fire poisoned the id and why the wanted fire was blocked.

**With the schema.** Each fire is a distinct `intent_id` with a `fire_seq`. Fire 4
emits `GateDecision{ gate: prep, passed: false, reason: missing-prep }` then
`OutcomeRecorded{ Rejected }`. Fires 5–6 emit `ReplayGuarded` (the 409) — a
**distinct event type**, not an error. Filtering by `trade_id` and reading the
`fire_seq` / event-type column shows the poison-then-block sequence directly.
✅ **Caught** — and note the underlying bug is already fixed (`seen_decision`
only marks on `Ok`), so the schema's job here is *making the fix verifiable* via
replay rather than finding the bug.

---

## 2. Bug #6 — a `KvTransition` that failed ("wanted to cancel, couldn't") — ✅

**What happened.** The worker intended a KV state change that failed; the
swallowed failure left the system in a state the operator had to reconstruct from
scattered state-error log strings.

**Diagnosis cost then.** Archaeology across error strings to infer that an
intended transition didn't happen.

**With the schema.** This is the case the schema was *specifically* hardened for:
`KvTransition` carries `success: bool` + `error`. A failed transition is a
first-class, queryable record — "the bot wanted to go from X→Y and couldn't,
here's the error." ✅ **Caught.** This is the single highest-value debugging
addition.

---

## 3. Swallowed-cancel stop-outs — ✅ (depends on one rule)

**What happened.** A pending order's cancel was swallowed; the position later
stopped out, and the operator couldn't reconstruct that the stop-out was a
*consequence* of a cancel that never landed.

**With the schema.** Two events make this legible: the failed
`KvTransition`/`BrokerCall` for the cancel (with `success: false`), and a
`PositionClosed{ exit_type: SL }` emitted **even though the worker didn't
initiate the close**. ✅ **Caught — but load-bearing on the rule that
`PositionClosed` is emitted for worker-uninitiated stop-outs.** If that rule
slips, this regresses to ❌. Flagged in the schema as critical for exactly this
reason.

---

## 4. `#19-10` "entry too close to market" (GBP/NZD, DOW; 2026-06-03/04) — ⚠️→✅

**What happened.** A buy-stop trigger already overtaken by price → TN rejects
`#19-10`. The worker **flattens the distinct error** into generic
`OrderRejected` → opaque `502 entry failed: broker rejected the order`. The
GBP/NZD ~10R setup was lost; a DOW refire luckily rescued itself.

**Diagnosis cost then.** Per the bug brief: a full investigation to discover the
rejection was specifically too-close, because the error identity was lost at
`map_place_error`.

**With the schema alone — ⚠️.** The schema records `BrokerResponse{ error }` and
`OutcomeRecorded{ Failed, 502 }` — so you'd see *a* broker failure with whatever
error string survives. **But the schema can't recover information the code threw
away upstream.** If `map_place_error` collapses `EntryTooCloseToMarket` →
`OrderRejected` before the worker ever sees it, the event faithfully records
"OrderRejected" — the granularity is gone before recording.

**The lesson — this is the most important finding of the audit.** Recording is
**downstream of error fidelity**. The schema makes *observed* facts queryable; it
cannot reconstruct facts the broker-adapter discarded. So **step 1 of the bug
brief (plumb the distinct `EntryTooCloseToMarket` variant through) is a
prerequisite for recordability**, not an alternative to it. Once that variant
surfaces a distinct outcome string, `BrokerResponse{ error: too-close-to-market }`
makes it a log-grep → ✅.

> **Partial progress already in tree (2026-06-14):** `core/src/broker.rs:138`
> *already* has `EntryError::EntryTooCloseToMarket` with a Display string. So the
> core layer is done — the remaining flattening is upstream in
> `broker-tradenation`'s `map_place_error` (per the bug brief). Worth confirming
> the whole chain before relying on it for recording.

> **Schema implication:** add a note that `BrokerResponse.error` should carry the
> *most specific* error variant available, and that flattening errors in the
> broker adapter directly degrades recordability. Error-fidelity and
> recordability are the same project.

---

## 5. `too-low` / pcl-exhausted veto closed a winner (trade 046, CHF/JPY) — ✅

**What happened.** The pcl-exhausted veto was mis-tagged `level: ClosePositions`
(should be `StopNextEntry`), so it flat-closed an in-profit short 74.5% of the way
to TP. Booked +3.76R instead of +5.05R.

**Diagnosis cost then.** Reconciling TV CSV with CF JSON to prove the close came
from *this* veto and not the reversal-close or the news window — three candidate
causes to rule out.

**With the schema.** The veto fire emits `GateDecision{ gate: veto, name:
too-low, passed: true }` carrying the **`level` field**, immediately followed by
`BrokerCall{ close_positions }` → `PositionClosed{ exit_type: veto-close,
position_id, parent_event_id → the GateDecision }`. The `parent_event_id` chain
makes "this veto caused this close" explicit — no ruling-out of the other two
candidates needed. ✅ **Caught**, *provided* `GateDecision` for vetos records the
`level` and `PositionClosed.exit_type` distinguishes `veto-close` from `SL`/`TP`.

> **Schema implication:** `exit_type` must include `veto-close` as a distinct
> value (already listed), and veto `GateDecision` must carry the `VetoLevel`. Add
> the latter to the variant notes.

---

## Summary

| incident | verdict | depends on |
|---|---|---|
| CHF/JPY mark-seen poison | ✅ | `ReplayGuarded` distinct from error; `fire_seq` |
| Bug #6 failed KV transition | ✅ | `KvTransition.success/error` |
| swallowed-cancel stop-out | ✅ | `PositionClosed` on worker-uninitiated stops |
| `#19-10` too-close | ⚠️→✅ | **upstream error-fidelity fix first** |
| pcl-exhausted closed winner | ✅ | veto `GateDecision` carries `level`; `exit_type: veto-close` |

**Two schema changes this audit surfaced:**

1. `BrokerResponse.error` should carry the **most specific** broker error variant
   — and the broker adapter must stop flattening errors (the `#19-10` lesson).
   *Error fidelity is a recordability prerequisite, not separate work.*
2. Veto `GateDecision` events must carry the **`VetoLevel`**, and `PositionClosed`
   must distinguish `veto-close` from `SL`/`TP` in `exit_type`.

**The headline finding:** 4 of 5 incidents become a single filtered query. The
one that doesn't (`#19-10`) fails for a reason the schema *can't* fix on its own —
which is itself the most useful thing the audit found: **recording quality is
capped by error-fidelity at the seam.**
