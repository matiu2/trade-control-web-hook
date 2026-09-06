# PLAN — fix the trade-142 order-loss chain, and close the replay↔live gaps

Companion to `BUG-cancelled-limit-order-resolves-unknown-blocks-reentry.md` and
`BROKER-EVIDENCE-trade-142-eurcad.md`.

**Incident plan id is `hs-eur-cad-08ca0693`** (account `m-and-w`, registered
2026-08-07 20:01), NOT `b6b71f6c` as the bug doc says — `b6b71f6c`/`27f43986`/
`b7ca58b5` are re-armed replay copies. The live evidence lives under the
`08ca0693` key in Postgres (`request_records`, intent_id
`plan-timeline-hs-eur-cad-08ca0693-2026-08-18T174914-b848`) — the journal only
reaches back to 2026-08-30, so Postgres is the only surviving record.

---

## What actually happened (corrected causal chain)

Reconstructed from the stored per-tick record against OANDA transactions
2316-2319. Two ruled-out theories are recorded at the bottom so nobody re-walks
them.

| # | Step | Verdict |
|---|---|---|
| 1 | `09-enter-qm` fires; its intent **is** `{"type":"limit"}` in the signed plan | by design |
| 2 | The limit rests unfilled — price moves away | real limit-entry risk, not a bug |
| 3 | Next fire's retry gate finds it `Pending` and **cancels it** to place a fresh one (`retry_gate.rs:227`) | by design (2316 cancelled 16:00:38, 2318 placed 16:00:41) |
| 4 | 17:00:01 — `05-enter` runs the gate **first** (`enter.rs:139`), which cancels 2318; **then** the prep gate rejects the fire (`enter.rs:204`, `prep-order-violated (retest)`) | **BUG A — this is the loss.** Order destroyed, nothing placed, no restore |
| 4b | That prep rejection was **spurious** — both preps carry an identical wall-clock `set_at`, so the strict `>` ordering check failed on correct geometry | **BUG C — the trigger.** See Stage 5 |
| 5 | 2318 is now cancelled-never-filled ⇒ `broker_trade_id` is `None` ⇒ resolves `Unknown` (`oanda.rs:632`) ⇒ all 12 later fires hard-rejected | **BUG B — prevents recovery** |

Fix A and the setup recovers on the next golden bar. Fix B only and the order is
still destroyed each time, but re-entry is possible afterward. **A is the one
that forfeited the trade, and it is still in the code.**

### Ruled out — do not re-investigate

- **The too-close stop→limit flip.** `place_entry_too_close_fallback`
  (`enter.rs:1232`) was NOT reached: `EntryTooCloseToMarket` is produced **only**
  by the TradeNation adapter (`broker-tradenation-adapter/src/lib.rs:803`).
  OANDA's `place_entry` returns `OrderRejected` for every rejection path and
  never this variant. The incident was on OANDA. Live placed limits because the
  QM intent *is* a limit.
- **`order_control/reprice.rs`.** It re-places via `run_enter(restore = true)`,
  which **bypasses the retry gate entirely** (`enter.rs:134`) and records no
  `EntryAttempt`, so it cannot produce this. It also logs loudly on a failed
  re-place (`reprice.rs:211`); no such line exists. Zero reprice activity in the
  journal window.

---

## The replay↔live gaps this exposed

Replay returns a healthy number for this plan and **cannot reproduce any of the
above**. Three independent reasons, each worth its own fix:

**Gap 1 — replay never fires the QM leg.** On the stored plan, every replay
placement is `05-enter` (a stop, filled next bar); live fired `09-enter-qm`
twice. The engine's `needs_confirmed` handling is shared
(`engine/src/evaluate.rs:1313,1436`) and `signal_confirmed` rides the
`FiredIntent`, so the leg *is* reachable offline — but `signal_confirmed`
appears **nowhere** in `cli/src/bin/replay_candles/`. Needs tracing: whether the
replay's signal source never sets it, or the confirmed-first scan
(`last_confirmed_enter_at`) advances differently offline.

**Gap 2 — replay's broker cannot model an unfilled resting limit's lifecycle.**
`ReplayBroker::place_entry` (`replay_broker.rs:961-991`) enforces only the risk
cap and the open-positions cap. There is no cancel-then-rejected-fire path, so
step 4 is structurally unreachable.

**Gap 3 — two independent `AttemptState` resolvers.** Live infers `Cancelled`
from `broker_trade_id.is_some()` (`oanda.rs:632`); replay reads a recorded
`cancelled: bool` (`replay_broker.rs:873`). They agree on 5 of 6 categories and
differ on exactly the one this incident produced. Nothing tests them against
each other.

**Consequence: re-running the fixture verifies none of these fixes.** The
conformance suite below is not a nice-to-have guard — it is the only test route.

---

## Plan

Ordered by what stops losing money first. Each stage is independently
committable, tests-first, and stays well under the 600-line change budget.

### Stage 1 — BUG A: never cancel an order for a fire that can still be rejected

The rail already exists in prose. `order_control/reprice.rs` module docs state
it explicitly ("**never cancel an order you cannot re-place**" — body verified
*before* the cancel, rails 2 and 3), and `pending_lifecycle` follows the same
store-first ordering. The retry gate simply does not honour it: it cancels on
the way to a placement that any later gate can veto.

**Fix:** in `run_enter`, move every gate that can reject a fire **ahead of**
`retry_gate::evaluate` — specifically the prep gate (`enter.rs:204`), and audit
the veto / cooldown / `allow_entry` / sizing gates for the same hazard. The gate
must be the **last** thing before placement, so a cancel is only ever issued
when the placement is otherwise certain to be attempted.

Prefer reordering over a compensating "re-place what we cancelled" path: an
un-cancel is another failure mode, and the ordering fix removes the window
entirely.

- Tests first: a fire that passes the retry gate but fails the prep gate must
  leave the prior resting order **untouched** at the broker.
- Regression: the existing cancel-then-place multi-shot path must keep working
  (2316→2318 is legitimate).
- Mutation-check the entry point, not the layer below — per
  `[[mutation_test_the_entry_point_not_just_the_layer_below]]`, assert against
  `run_enter`, since a guard one level down would mask a survivor.

### Stage 2 — BUG B: resolve cancellation from the broker, not from snapshot presence

`Unknown`'s fail-safe is correct and must not be weakened (Bug #11 —
`unknown_prior_attempt_rejects_failsafe` must stay green). **Narrow the input**
so a knowable fate stops arriving as `Unknown`.

**The bug doc's suggested mechanism does not work as written.** It proposes
`GET /v3/accounts/{id}/transactions/{id}` on the order id — but `broker_order_id`
is the *submission* transaction (2316 = `LIMIT_ORDER`); the cancel is a
**separate later** transaction (2317, carrying `orderID: 2316`). Fetching 2316
returns the submission and says nothing about its fate; finding the cancel means
scanning forward for a matching `*_CANCEL`, which is expensive on a ~5s cron.

**Use `GET /v3/accounts/{id}/orders/{orderSpecifier}` instead** — it returns the
order with a `state` field (`PENDING` / `FILLED` / `TRIGGERED` / `CANCELLED`).
One call, no scanning, unambiguous.

- `oanda-client` has no single-order GET today (it has `get_pending_orders` and
  `cancel_order`). Add it there, shaped like the existing methods.
- Then in `compute_attempt_state`, before the step-4 fallthrough: an order absent
  from pending/open/closed whose broker record says `CANCELLED` ⇒
  `AttemptState::Cancelled` (the gate `continue`s), not `Unknown`.
- Keep `Unknown` for the genuinely unresolvable: lookup failed, or state
  ambiguous.

### Stage 3 — conformance suite over `AttemptState` (closes Gap 3)

One scenario table, both resolvers, asserting identical output — the guard the
bug doc proposes, and the only thing that would have caught this.

- Case #1: **cancelled-never-filled** (the incident).
- Plus the five they already agree on: pending, open, closed-win, closed-loss,
  never-placed.
- Verify by mutation: revert Stage 2 and confirm case #1 goes red.

### Stage 4 — teach the replay broker the resting-order lifecycle (closes Gap 2)

Give `ReplayBroker` the missing arm so step 4 is reachable offline: a resting
order that the retry gate cancels, for a fire that is then rejected, must
disappear the way it does live. This is what makes the fixture corpus able to
exercise the resting-limit path at all.

Scope guard: only what is needed to model cancel-without-replace. Resist
re-simulating broker internals — `[[replay_sizing_gap_accepted]]` is the
precedent for naming a gap rather than faking it.

### Stage 5 — RESOLVED, and re-scoped: preps are stamped with WALL-CLOCK

**Gap 1's premise was wrong.** The replay DOES fire the QM leg: across all 452
strategy-v2 fixtures `09-enter-qm` fires 1684 times in 278 fixtures and places
216 orders in 178 — marginally more than `05-enter`'s 215. `signal_confirmed` is
computed inside the shared engine (`core/src/signals/state_machine.rs:98,261`),
not supplied by the harness, so it was never absent; it simply is not *named* in
`replay_candles/`.

**The real divergence is Bug C, and it sits AHEAD of Bug A in the causal chain.**

`handle_prep` stamps `set_at = now` — wall-clock (`core/src/dispatch/control.rs:193`)
— while the prep gate requires **strictly increasing** `set_at`
(`core/src/intent/prep_req.rs:123`). Live's cron hands **every bar closed since
the watermark** to one `evaluate_plan` call (`trade-control-cron/src/engine.rs:181`,
looped at `engine/src/evaluate.rs:284`) under a **single** `Utc::now()`. So a
multi-bar catch-up stamps both preps identically and the entry is rejected
`prep-order-violated` despite correct geometry.

**PROVEN against the incident, not inferred** — the two stored prep rows are
byte-identical to the microsecond:

```
break-and-close | 2026-08-08 00:07:05.759557+10
retest          | 2026-08-08 00:07:05.759557+10
```

So live's rejection of `05-enter` was a **false negative**. `05-enter` should
have placed; the QM limit filled the vacuum; and the spurious rejection is
exactly what made the retry gate's cancel (Bug A) destroy a resting order for
nothing. Bug C **triggers** Bug A.

Replay cannot reproduce it: one bar per `evaluate_plan` call, each with its own
`now` (`replay.rs:261,267`). `prep-order-violated` occurs **zero** times across
all 452 fixtures. Replay's correct verdict here is accidental immunity, not
fidelity — **no fixture is evidence about any timing-sensitive gate.**

**Fix:** stamp preps with the bar time (available on `verified.shell`), keeping
`now` for the TTL only. ~20 lines plus tests. Do NOT weaken the check to `>=` —
that permits a genuine same-bar retest and discards real ordering information.
Check the TTL and `is_prep_blocked` paths don't rely on `set_at` being wall-clock.

**Corpus blast radius — narrower than feared.** The QM columns are genuinely
exercised (`strategy-v2` QM limit: 132 orders in 98 of 226 fixtures;
`strategy-v2-qm-market`: 84 in 80), so the entry-rule comparison, the `skip-bcr`
choice and the broker/asset-class findings all stand. Two real caveats: any claim
resting on the *relative* frequency of `05-enter` vs `09-enter-qm` is biased
toward `05-enter` (rule order gives the stop leg the tie —
`engine/src/evaluate.rs:816` — and the QM leg then eats `trade-already-open`),
and live's spurious prep rejections hand the QM leg extra wins offline replay
never grants, so replay **under-counts QM participation relative to live**.

### Stage 6 — placement is not a fill (operator honesty)

`entered: order=2316` is written at placement (`enter.rs:954`), so a resting
limit that never fills is indistinguishable from a winner. This is how the
incident reached the journal as **WIN +2.63R**.

- `placed: order=<id>` on acceptance; `entered: order=<id> fill=<price>` only on
  a real fill. Same reasoning as the v135 CLI fix.
- Surface a cancelled-never-filled attempt to the operator: this plan sat
  blocked for 11 days and the only visible signal was `close-failed`, which
  reads as a *closing* problem, not an entry one.
- Correct `books/demo-journal/src/trade-142-eurcad-h1-hs-win-2p63r.md` — it
  records a WIN for a trade that never opened. **Note the filename asserts the
  wrong outcome too.**

### Stage 7 — `ORDER_CANCEL_REJECT: ORDER_DOESNT_EXIST` storms

`BROKER-EVIDENCE` §6 logs bursts of 2-3 cancels one second apart against orders
that no longer exist (10 of 17 transactions in range 2300-2316 are failures).
Almost certainly the same gate arm cancelling an order the broker no longer has.
Confirm, then fold into Stage 1's ordering fix if it shares the root.

---

## Acceptance

- A fire that passes the retry gate but fails a later gate leaves the prior
  resting order intact at the broker (Stage 1).
- A placed-then-cancelled-without-filling attempt resolves `Cancelled`, and the
  gate proceeds to the next attempt rather than `prior-attempt-unknown`
  (Stage 2).
- `Unknown` still blocks re-entry when the broker genuinely cannot confirm
  state — `retry_gate.rs::unknown_prior_attempt_rejects_failsafe` stays green.
- Both `AttemptState` resolvers agree on all six categories (Stage 3).
- The log distinguishes a placed-but-unfilled order from a filled one (Stage 6).

**Not an acceptance criterion:** any particular replay R-number. The bug doc
cites +2.63R as as-designed, but a replay of the stored plan returns **+3.97R**
(different SL-spread-floor sizing off a 1.6p vs 2.5p measured spread), and the
re-armed copies return +2.63R from a *different* plan. Since replay cannot reach
the failure state, no fixture number confirms or refutes these fixes.
