# TODO — extract multi-shot retry decision into shared `core`, use it in replay

## Why
`replay-candles` fires the H&S enter once, the engine goes `Phase::Done`, and the
replay stops — so it never shows the **multi-shot re-entry** the live worker would
do (place → fill → stop out → re-enter on the next golden signal, up to
`max_retries`). The whole multi-shot mechanism lives in `src/retry_gate.rs`
(worker-only, KV + broker I/O). The engine (`evaluate_plan`) knows nothing about
it. So the replay is **unfaithful** to the worker for any multi-shot setup.

Verified on NZD/CHF 2026-06-19 (`/tmp/nzd-chf-new.json`, `max_retries: 5`):
07:30 golden short pinbar enters (CORRECT per strategy), stops out 09:00, and the
trade should **re-enter at the 13:00 golden short pinbar** — but replay shows 1
fire + Done. The 13:00 bar IS golden (`fires=true` in the latch dump); the plan
just retired instead of re-arming.

## REVISED design (settled 2026-06-22, user's async-broker model)

Do NOT extract a pure decision fn. Instead share the **whole async gate** and
swap only the broker (the user's insight: re-entry must really ask the broker if
the prior position closed; in replay the "broker" approximates that from candles).

- **Step 1 — DONE (commit 940c948):** `retry_gate` moved `src/` → `core/` (it was
  already generic over `<B: Broker, S: StateStore>`; only `rlog!` macros blocked
  it → now `tracing`). Worker calls `core::retry_gate::evaluate(real_broker, kv)`;
  18 gate + 211 worker tests green; wasm compiles.
- **Step 2 — replay fake broker + multi-shot loop (THIS step):**
  - `ReplayBroker` impls `Broker`. Only the retry-gate methods do real work:
    - `lookup_attempt_state(order_id)`: find the synthetic `EntryAttempt`,
      `simulate_fill` it against candles **up to the asking bar** (user choice:
      time-accurate), map `SimOutcome` → `AttemptState`:
      `StoppedOut`→`ClosedLossOrBreakeven`, `TookProfit`→`ClosedWin`,
      `FilledOpen`→`OpenPosition{trade_id}`, `NeverFilled`→`Pending`/`Cancelled`.
    - `list_open_positions`: synthetic positions for attempts still open by now.
    - `cancel_order`: mark the synthetic attempt cancelled.
    - everything else (`place_entry`, `amend_stop`, `get_candles`, …): unreachable
      stub / no-op (replay never places real orders).
  - `MemStateStore` (already in core, `test-support`) is the ledger.
  - Replay loop: on an enter fire, don't `break` on `Done`. Call
    `retry_gate::evaluate(replay_broker, mem_store, intent, shell)`; on `Proceed`
    `simulate_fill` the entry, `record_placement` a synthetic attempt, re-arm to
    `AwaitEntry`, keep scanning. Stop on gate reject (cap/open) or window end.

## Original (superseded) design
Extract the **pure decision** of `retry_gate::evaluate` into `core` so the worker
and the replay call the SAME collapse/cap logic. I/O stays per-consumer.

- **Pure core (`core/src/retry_decision.rs`):**
  `retry_decision(attempts_with_state: &[(EntryAttempt, AttemptState)],
  open_positions: &[OpenPosition], instrument, max_retries) -> RetryDecision`
  - `RetryDecision::Proceed { next_attempt_no }` | `Reject { kind }` (+ a mapping
    to status/message/outcome on the worker side).
  - Encodes: open-position backstop match, newest-first walk (Open/Pending/
    Unknown → reject; Closed*/Cancelled → skip older), cap check.
  - KV-free, broker-free, fully unit-tested.
- **Worker (`src/retry_gate.rs`):** keep `evaluate` as the async shell — it does
  the KV reads + `lookup_attempt_state` (incl. the Pending→cancel→re-lookup race)
  + `list_open_positions`, then calls `retry_decision` for the verdict. Behaviour
  byte-identical; existing worker tests must stay green.
- **Replay:** in-memory `Vec<(EntryAttempt, AttemptState)>` ledger. After each
  simulated fill→exit, append a synthetic attempt with a `Closed*` state; on the
  next golden signal call `retry_decision`; if `Proceed`, simulate the next fill.
  Loop until cap or window end. Requires the replay loop to NOT `break` on the
  first enter `Done` for a multi-shot enter — re-arm to `AwaitEntry`.

## Steps
- [ ] `core/src/retry_decision.rs`: `RetryDecision` + `RejectKind` + pure
      `retry_decision(...)`. Declare `mod`/`pub use` in `core/src/lib.rs`.
- [ ] Unit tests in core: empty→Proceed#1; one closed→Proceed#2; open→Reject;
      pending→(worker cancels, but pure fn sees it as block? — decide: pure fn
      treats Pending as "caller handles"); cap reached→Reject; backstop match→Reject.
- [ ] Refactor `src/retry_gate.rs::evaluate` to gather states then call
      `retry_decision`. Map `RejectKind` → existing `{status,message,outcome}`.
      Keep the Pending→cancel→re-lookup race in the I/O shell.
- [ ] Worker test suite green (byte-identical single-shot + multi-shot tests).
- [ ] Replay: multi-shot ledger + loop past `Done`, calling `retry_decision`.
- [ ] Acceptance: `replay-candles-dev --plan=/tmp/nzd-chf-new.json --simulate true
      --source tradenation` shows 07:30 enter→SL, then 13:00 re-enter, riding (or
      its real outcome) — NOT a lone 07:30 stop-out + Done.
- [ ] clippy + fmt. Worker + engine + cli test suites green.
- [ ] README: note replay now models multi-shot.

## Open question for the pure/IO split
`Pending` requires a broker `cancel_order` + possible re-lookup (a race). That's
I/O — it can't be pure. Decision: the pure `retry_decision` is called AFTER the
shell has resolved each attempt to a *terminal* state for decision purposes
(the shell does the cancel and reports the post-cancel state, or a Pending that
it couldn't cancel). i.e. the shell flattens the race; the pure fn sees only
states it can decide on without further I/O. Document this seam precisely.
