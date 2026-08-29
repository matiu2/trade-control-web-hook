# TODO — entry rate/size on the timeline + broker settlement on archive

Branch: `feat/entry-rate-size-and-settlement`
Worktree: `../trade-control-entry-detail` (sibling — path-dep rule)

## Why

Two operator asks off the `journal-staging` timeline:

1. The `fired 05-enter (enter)` line should show **the rate and the trade size**.
2. Once a trade is **archived**, the server should pull the broker's
   **activity + transaction** info for the account and fold it into the log.

## Findings so far (verified in code, not prose)

### Where the timeline line comes from

- `journal/src/timeline.rs:91-94` formats `fired {rule_id} ({action})` from
  `eval.fired[].intent` — the *intent*, not the fill.
- The placement result rides `dispatch_outcomes[].outcome`
  (`core/src/tick_bundle.rs:113`), a bare string built at
  `core/src/dispatch/enter.rs:954` as `entered: order={order_id}`.
- So enriching that string surfaces in the timeline with **no journal change**.

### Rate and size are computed then discarded

- `Broker::place_entry` returns `Result<String, EntryError>` — order id only
  (`core/src/broker.rs:248`).
- OANDA computes `units` at `broker-oanda/src/oanda.rs:146-190` and only
  `tracing::info!`s it (line 195).
- **TradeNation computes stake entirely upstream** — the adapter just forwards
  (`broker-tradenation-adapter/src/lib.rs:53-64`). Units are *not* in scope
  locally at all for TN.
- `EntryAttempt` (`core/src/state.rs:163`) stores `stop_loss_price`, `pip_size`,
  `cancel_at` — but **no entry price and no units**.

⚠️ `ResolvedEntry::reference_price()` is the **requested** price (the trigger for
a stop/limit), NOT the fill. Slippage means these differ. Label honestly.

### Archive triggers — operator was RIGHT, my first read was wrong

`Phase::Done` → cron archives (`trade-control-cron/src/engine.rs:420-447`).

| Trigger | Archives? | Position closed first? |
|---|---|---|
| `too-low` / pcl-exhausted (`StopNextEntry`) | **No** — sets `entries_blocked` | n/a |
| `too-high` / invalidation (`ClosePositions`) | Yes | Yes (CLOSE-VETO) |
| `trade-expiry` (`CancelPending`) | Yes | Yes |
| M/W cancel/abort/overshoot | Yes | Yes |
| **single-shot enter, first fire** | **Yes, immediately** | **NO** ⚠️ |

- `StopNextEntry` not retiring is deliberate and load-bearing —
  `engine/src/evaluate.rs:2946` + the XAU_XAG H1 2026-07-21 regression noted at
  `evaluate.rs:2930-2932`. Confirmed by the operator's own AUD/CAD timeline:
  `01-veto-too-high` at 11:00 did NOT archive; the plan ran on to a
  `07-close-on-sr-reversal` a day later and ended at `02-veto-trade-expiry`.
- **The hole:** `engine/src/evaluate.rs:1048-1055` sets `Phase::Done` on the bar
  a single-shot (`max_retries == Static(0)`) enter *fires* — before fill, let
  alone close. Violates the invariant "archive only after all attempts closed".

### Broker history APIs available

- **TradeNation: already implemented, never wired up.**
  `tradenation-api/src/activity.rs` — `ActivityRecord { price, stake, ... }`
  (literally the fill rate + size) via `get_activity` / `get_all_activity`.
  `tradenation-api/src/transactions.rs` — settled ledger, `get_all_transactions`.
  Neither is reachable through `broker-tradenation` or the adapter.
- **OANDA: partial.** `oanda_client::trades::Trade` already carries `price`
  (execution), `initial_units`, `average_close_price`, `close_time`,
  `realized_pl`, `financing`, `closing_transaction_ids`.
  `lookup_attempt_state` (`broker-oanda/src/oanda.rs:339`) already queries
  `TradeState::Closed` and throws the numbers away, keeping only win/loss.
  **No `/v3/accounts/{id}/transactions` endpoint exists in `oanda-client`** —
  the full ledger needs that endpoint added (separate repo/submodule).
- `Broker` trait has **no** `list_transactions` / `get_activity` method.

### Investigation results (single-shot Done-at-fire)

**It is near-unreachable in production.**

- `tv-arm/src/hs_resolve.rs:306-310` defaults `max_retries` to **5**; `--strategy-v2`
  *rejects* `--max-retries 0` (`args.rs:936-943`).
- Only single-shot producers: M/W (`tv-arm/src/mw_resolve.rs:266-268`, deliberate)
  and the interactive `build-trade` wizard (`cli/src/trade_patterns.rs:1041`).
- **Corpus: 874 plans, 1308 enter rules, ALL `max_retries: 5`. Zero single-shot.**

**It was never a design decision.** `git log -S "phase = Phase::Done"` returns one
commit — `e3a76aa`, the engine's original naive spine. `83333fa` carved out
multi-shot and asserted "for a single-shot enter that's correct" without
justification.

**Dropping the `Done` is safe — `fired` is what prevents re-firing.**
`evaluate.rs:835-837` skips latched rules *before* `evaluate_one_entry`, and the
`fired.insert` sits on the line above the `Done` (`:1053-1054`). Keep the insert,
drop the `Done`.

**What it unlocks (all desirable):** the per-position reversal-close becomes armed
for single-shot plans (today it can *never* fire — same defect class as the
XAU_XAG incident); invalidation vetos keep ticking; the pending-order lifecycle
keeps running (`replay.rs:2608-2612` documents the break skipping spread-hour
cancel/restore entirely).

⚠️ **Two hard constraints found:**

1. **The engine is deliberately broker-free.** `core::position_view` / `OpenSet` /
   the `positions` param on `evaluate_plan` were **removed in v66**. The literal
   "all attempts closed" invariant CANNOT live in `evaluate_plan` — it needs a
   cron sweep. Reintroducing a broker there regresses a thrice-fixed bug.
2. **Plan rows have NO TTL** (`core/src/state.rs:1080-1085`) — `Phase::Done` is the
   only GC. Any fix must guarantee a terminal path or plans leak and tick every
   ~5s forever. `02-veto-trade-expiry` (`ClosePositions`) is the backstop, but it
   needs a bar to *close* past the epoch (guards test `candle.time`, not wall
   clock — `evaluate.rs:601-603`).

⚠️ `not_after` is **NOT enforced by the engine** — it appears only in a test
fixture. Comments at `:336`, `:731`, `:1024`, `:2937` claiming otherwise are wrong.
The only retirement is `02-veto-trade-expiry`.

**Replay parity:** `replay.rs:674-677` `break`s the candle loop on `eval.done`
(its only early exit). Golden compares `done` AND `final_phase` exactly
(`golden_eq.rs:131-139`). Corpus terminating fires: `02-veto-trade-expiry` 471,
`01-veto-too-high` 213, `01-veto-too-low` 129, `06-close-on-reversal` 3 —
**none reach Done via the enter**, so the change moves ZERO fixtures. Safe, but
also *unvalidated by the corpus* → needs its own targeted test.

**Tests to update (~7):** `evaluate.rs:6668`, `:6744`, `:6768`, `:7347`
(`assert!(eval.done, "single-shot retires the plan on its fire")` — the explicit
guard), `:4885`; verify `:5648`, `:8694`. Multi-shot twins already assert
`!eval.done` and stay green.

## Operator decisions taken

- Enter line: **widen `place_entry` to return units** — `reversals` (the account
  in the timeline) is **TradeNation**, where stake is computed upstream and never
  returned, so string-only would give rate but NO size on that very account.
  Corpus: tradenation/reversals 774 enters, oanda/m-and-w 534.
- Archive settlement: **trade-level + full transaction ledger**.
- Single-shot Done-at-fire: **fix it** — don't retire until the position closed.

## Plan

- [x] **1. Investigate single-shot retirement** — done, see above.
- [ ] **2. Widen `place_entry`** to return `Placement { order_id, units, price }`.
      Impls: `broker-oanda/src/lib.rs:54` (units in scope at `oanda.rs:190`),
      `broker-tradenation-adapter/src/lib.rs:30` (needs upstream stake), plus
      test spies in `retry_gate.rs:561`, `order_control/reprice.rs:510`,
      `order_control/promote.rs:319`, `core/src/broker.rs:441`.
- [ ] **3. Enrich the enter outcome string** at `core/src/dispatch/enter.rs:954`
      with rate + size. Label the price honestly — for a stop/limit it is the
      **trigger**, not the fill (slippage).
- [ ] **4. Fix single-shot Done-at-fire** (`engine/src/evaluate.rs:1054`) — drop
      the `Done`, keep the `fired.insert`. Update the ~7 tests.
- [ ] **5. Broker settlement on archive** — new `Broker` trait method; wire TN's
      existing `get_all_activity`/`get_all_transactions`; add OANDA's
      `/v3/accounts/{id}/transactions` endpoint to `oanda-client` (separate repo).
- [ ] **6. Persist settlement on `ArchivedPlan`** (jsonb, `#[serde(default)]`,
      no migration — matches the `EntryAttempt` additive pattern).
- [ ] **7. Surface it in the journal timeline output.**

## Gates (per CLAUDE.md)

- Tests first; prove each test can fail (mutate the source, confirm red).
- `cargo clippy` + `cargo fmt` before each commit.
- Keep changes < ~600 lines each; commit + push as each lands.
- Strategy changes must land in **both** replayer and worker.
