# BUG — a cancelled limit order that never filled resolves to `Unknown`, permanently blocking re-entry

> **STATUS 2026-09-06: root cause established; fixes on branch
> `fix/trade-142-order-loss-and-parity`. This document's diagnosis was PARTLY
> WRONG — read `PLAN-trade-142-parity-and-order-loss.md` for the corrected
> account.** Summary of what changed:
>
> - **The plan id below is wrong.** The incident plan is **`hs-eur-cad-08ca0693`**
>   (8 request_records); `hs-eur-cad-b6b71f6c` has **zero** — it is a re-armed
>   copy. `tv-arm` mints a fresh id per arm.
> - **The `Unknown` resolver is real but THIRD in the chain**, not the root. It
>   blocked *recovery*; it did not cause the loss.
> - **The actual root is prep timestamping** (wall-clock, not bar time), which
>   spuriously rejected `05-enter` with `prep-order-violated` on correct
>   geometry — proven: both prep rows are byte-identical to the microsecond.
>   That rejection then triggered...
> - **...the retry gate cancelling a resting order for a fire it then rejected.**
>   That is what destroyed order 2318. It affected **thirteen** gates, not one.
> - **The suggested fix below does not work as written** — see "Suggested fix"
>   note. `get_transaction(order_id)` returns the *submission*, not the cancel.
> - **"Why live and replay disagree" is wrong.** Replay *does* fire the QM leg
>   (1684 fires, 216 placements across 452 fixtures). The divergence is that
>   replay dispatches one bar per tick and so can never reproduce a
>   cron-catch-up timing bug.
> - **The +2.63R acceptance figure is not reliable.** A replay of the *stored*
>   plan returns +3.97R; the +2.63R came from a re-armed copy. Replay cannot
>   reach the failure state, so no fixture number confirms these fixes.


**Found:** 2026-09-06, journaling EUR/CAD H1 H&S short (demo-journal trade 142).
**Severity:** High — a whole setup was silently forfeited (+2.63R as-designed,
0R live) and the journal recorded a **WIN** for a trade that never opened.
**Raw broker data:** `BROKER-EVIDENCE-trade-142-eurcad.md` — every OANDA
transaction quoted verbatim, plus the control cases proving the account was healthy.
**Distinct from** `BUG-market-entry-no-broker-confirmation-trail.md` (fixed v135):
that was the manual `--market-entry` path. This is the **engine** `09-enter-qm`
path, and the root cause is in the OANDA attempt-state resolver.

---

## Symptom

Plan `hs-eur-cad-b6b71f6c` (EUR_CAD H1, account `m-and-w`, armed
`--strategy-v2 --qm-entry=market`) logged two successful entries:

```
2026-08-08 00:59 • fired 09-enter-qm (enter) → entered: order=2316
2026-08-08 02:00 • fired 09-enter-qm (enter) → entered: order=2318
```

Then rejected **twelve** subsequent entry fires:

```
2026-08-08 06:00 → 2026-08-11 15:00
  05-enter    × 8  → rejected: prior-attempt-unknown
  09-enter-qm × 4  → rejected: prior-attempt-unknown
```

…and failed **three** close attempts:

```
2026-08-08 04:59  07-close-on-sr-reversal → close-failed
2026-08-17 23:00  06-close-on-reversal    → close-failed
2026-08-19 00:00  02-veto-trade-expiry    → cancelled=0 closed=failed
```

**No position ever existed at the broker.** The replay of the same plan returns
**+2.63R** (one scratch + one TP), so the setup was good; the live run banked 0R.

## Broker ground truth

From OANDA's transaction log (account `101-011-31142393-003`):

```
2316  15:00:11Z  LIMIT_ORDER   EUR_CAD  -865,150   accepted, no failure
2317  16:00:38Z  ORDER_CANCEL  orderID=2316  reason=CLIENT_REQUEST
2318  16:00:41Z  LIMIT_ORDER   EUR_CAD  -672,809   accepted, no failure
2319  17:00:23Z  ORDER_CANCEL  orderID=2318  reason=CLIENT_REQUEST
2320  2026-08-10 13:00:03Z  STOP_ORDER  NZD_CAD  (unrelated, fills fine)
```

Both orders were **accepted** (`is_failure: false`, no reject reason — not
margin, not units, not halted), rested for ~1 hour each, and were **cancelled by
us** (`CLIENT_REQUEST` = our own API call). Neither ever filled; `get_trade(2316)`
and `get_trade(2318)` both return `NO_SUCH_TRADE`.

Ruled out: market closed (both were **Friday 11:00 / 12:00 New York**, peak
liquidity), broker rejection, sizing/min-ratio, dead credentials (the same
account filled NZD/CAD, XCU/USD, AU200 and FR40 from 08-10 onward).

## Why live and replay disagree — the shared-code question

The retry gate is **fully shared**: both systems run the same `retry_gate.rs`, and
both treat `Cancelled` as "continue to the next attempt" and `Unknown` as "hard
reject". That layer has real parity.

The divergence is one level below it. `AttemptState` is produced by **two
independent implementations** that agree on every case except this one:

| | Live (`broker-oanda/src/oanda.rs:632`) | Replay (`replay_broker.rs:873`) |
|---|---|---|
| How "cancelled" is known | **inferred** from `broker_trade_id.is_some()` | **recorded** — explicit `cancelled: bool` on the resting order |
| Cancelled, never filled | `broker_trade_id` is `None` → `Unknown` ✗ | flag set → `Cancelled` ✓ |

Replay cancels the order *in-process* and writes the fact down
(`replay_broker.rs:48-51`: "Set once the gate cancels this resting order... A
cancelled attempt resolves to `Cancelled` regardless of the price path"). It knows,
because it did it.

Live cannot know. `compute_attempt_state` receives only `pending`, `open` and
`closed` — three snapshots of **current** broker state. An order cancelled before
filling appears in none of them; it has simply vanished. So the function guesses
from the only proxy it has (`broker_trade_id`), which is set only for attempts that
reached an open position — and therefore answers "no" for precisely this case.

**This is structural, not a coding slip.** Cancellation is a historical event, and
every input the live resolver gets is present-tense. The information is not
recoverable from those three lists at any level of care. That is why the fix must
read the transaction record rather than tighten the inference.

### Why the fixture corpus never caught it

The two resolvers agree on pending, open, closed-win, closed-loss and never-placed
— five of six categories. They differ only on cancelled-never-filled, which replay
produces solely via the spread-hour supersede path. Live produced it here through
per-bar re-pricing. 60+ fixtures, none exercising the one divergent input.

The replay broker's documented shadow-parity assertions (`replay_broker.rs:858`,
"shadow-parity asserted vs `resolve` bar-by-bar through S3-S7") compare replay's
held model against replay's own re-sim — not against the live OANDA resolver.
Nothing tests `oanda.rs`'s four-step algorithm against `replay_broker.rs`'s
five-branch one on the same scenario.

**Suggested guard:** a shared conformance test-suite over `AttemptState`
resolution — one scenario table, both implementations, asserting identical output.
Cancelled-never-filled is case #1.

## Root cause

`broker-oanda/src/oanda.rs:632-641`, the fallthrough of the attempt-state
resolver:

```rust
// 4. Nowhere to be found. Distinguish so logs can tell us
//    whether we lost a snapshot or never had one.
if broker_trade_id.is_some() {
    AttemptState::Cancelled
} else {
    AttemptState::Unknown
}
```

`broker_trade_id` is only ever set **when an attempt reaches an open position**
(`retry_gate.rs:275-287` snapshots it from `AttemptState::OpenPosition`). An
order that is placed and then cancelled **without ever filling** therefore never
has one — so it lands in the `else` and resolves to `Unknown`.

But `Unknown` is defined as *"the broker couldn't confirm this attempt"*, and
`retry_gate.rs:302-323` deliberately fails **safe** on it:

```rust
Ok(AttemptState::Unknown) => {
    // ... Fail SAFE — an unresolvable prior attempt blocks the re-entry.
    return RetryGateOutcome::Rejected { ... "rejected: prior-attempt-unknown" };
}
```

That fail-safe is correct for its intended case (Bug #11 — a still-open TN
position whose order id had drifted). It is **wrong here**: the order's fate is
not unknown at all. OANDA has an explicit `ORDER_CANCEL` transaction for it. The
resolver simply never looks at transactions, and infers "cancelled" from the
presence of a snapshot rather than from the broker record.

So a never-filled cancelled order is misclassified as unresolvable, and because
`AttemptState::Cancelled` is a collapsed state that `continue`s to the next-older
attempt (`retry_gate.rs:294-300`) while `Unknown` returns immediately, **every
subsequent entry on that plan is blocked for the life of the plan.**

The three `close-failed` events are downstream of the same misconception: the
engine believed it held a position, so it kept trying to flatten nothing.

## Why the log said `entered:`

`entered: order=2316` is recorded at **placement**, not at fill. A resting limit
order that never fills produces the identical log line to one that fills. That is
the same class of overstatement fixed in v135 for the manual path, but the engine
path still has it. From the log alone, a forfeited setup is indistinguishable from
a winning one — which is how this reached the journal as **WIN +2.63R**.

## Open question — who cancelled, and why no re-place?

`CLIENT_REQUEST` means our system issued the cancels, ~1 hour after each placement
and within seconds of the next H1 bar close (15:00:11 → cancelled 16:00:38, next
order 16:00:41 → cancelled 17:00:23). That cadence looks like deliberate per-bar
re-pricing (`order_control/reprice.rs`?) — cancel the stale limit, place a fresh
one at the new level.

If so, the sequence broke on the **third** iteration: 2318 was cancelled at
17:00:23Z and **no replacement was ever placed**. Worth checking whether the
re-price path can cancel-then-fail-to-submit, and whether it records an attempt
row for the cancelled order that then poisons the retry gate.

That is the second half of this bug and I have not traced it — the resolver
misclassification above is confirmed from the code, this part is inferred from
timing.

## Suggested fix

> **⚠️ The mechanism proposed in item 1 does not work.** `broker_order_id` is the
> *submission* transaction (2316 = `LIMIT_ORDER`); the cancel is a **separate,
> later** transaction (2317, carrying `orderID: 2316`). Fetching 2316 returns the
> submission and says nothing about its fate — finding the cancel means scanning
> forward for a matching `*_CANCEL`, expensive on a ~5s cron. The shipped fix uses
> **`GET /v3/accounts/{id}/orders/{orderSpecifier}`**, which returns the order's
> `state` (`PENDING`/`FILLED`/`TRIGGERED`/`CANCELLED`) in one call. Verified
> against the live practice account: orders 2316 and 2318 both return
> `state: CANCELLED`.

**1. Resolve cancellation from the broker, not from snapshot presence.**
`AttemptState::Cancelled` should be returned when the broker says the order was
cancelled. OANDA exposes this directly:

```
GET /v3/accounts/{id}/transactions/{id}        -> type: ORDER_CANCEL, orderID: ...
GET /v3/accounts/{id}/transactions/idrange     -> walk a range
```

(The sibling `oanda-mcp` crate just added `get_transaction` / `list_transactions`
over `oanda-client`; the same call belongs here.)

Concretely: before falling through to step 4, look up the order's transaction. If
it is `ORDER_CANCEL` — or if the order id is absent from pending/open **and** its
transaction shows a terminal non-fill — return `Cancelled`, which lets the gate
`continue` to the next attempt instead of hard-blocking.

**2. Keep `Unknown` for genuinely unresolvable cases only.** The fail-safe is
right; it is just being reached by a case that is fully knowable. Narrowing the
input is the fix, not weakening the guard.

**3. Do not log `entered:` for an unfilled resting order.** Distinguish
placement from fill — e.g. `placed: order=<id>` on acceptance, `entered:
order=<id> fill=<price>` only on `ORDER_FILL`. Same reasoning as the v135 CLI fix.

**4. Consider surfacing a cancelled-never-filled attempt to the operator.** This
plan sat blocked for 11 days and reported nothing wrong; the only visible signal
was `close-failed`, which reads as a closing problem, not an entry one.

## Acceptance

- An attempt whose order was placed then cancelled without filling resolves to
  `AttemptState::Cancelled`, and the retry gate proceeds to the next attempt
  rather than returning `prior-attempt-unknown`.
- Replaying plan `hs-eur-cad-b6b71f6c` over 2026-08-07 → 08-18 no longer blocks
  the post-08-08 entries. (Fixture:
  `replay-fixtures/eur-cad-h1-2026-08-07-strategy-v2-qm-market-news-off`, expected
  **+2.63R**.)
- `Unknown` still blocks re-entry when the broker genuinely cannot confirm state
  (existing Bug #11 test must keep passing:
  `retry_gate.rs::unknown_prior_attempt_rejects_failsafe`).
- Log distinguishes a placed-but-unfilled order from a filled one.

## Evidence

- Resolver: `broker-oanda/src/oanda.rs:595-641` (step 4 at 632-641)
- Gate: `core/src/retry_gate.rs:275-323` (`Unknown` arm at 302-323; `Cancelled`
  `continue` at 294-300; `broker_trade_id` snapshot at 275-287)
- Plan: `hs-eur-cad-b6b71f6c`, account `m-and-w`, OANDA practice
  `101-011-31142393-003`
- Broker transactions 2316-2321 (quoted above); `lastTransactionID` 2453
- Trade id sequence shows the gap: `2303 (07-25) → [2316, 2318 never traded] →
  2321 (08-10)`
- Journal entry needing correction:
  `books/demo-journal/src/trade-142-eurcad-h1-hs-win-2p63r.md`
