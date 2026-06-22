# Bug #13 — cron-engine retires a plan on a `resolve-failed` enter

Report: `trading-tax-tracker/analysis-data/book/src/bug-013-cron-engine-resolve-failed-retires-plan.md`

## Root cause (confirmed in code)

`engine/src/evaluate.rs::evaluate_entry` latches a single-shot (`FireMode::Once`)
enter and sets `phase = Done` the moment the trigger *fires*, **before** the
bracket is resolved. Resolution (and the broker placement) happen later in the
worker's `dispatch_fired → run_enter`, whose `resolve-failed` outcome never
feeds back into the pure FSM. So a degenerate (zeros) bracket still tears the
plan down → its veto rules (valid hours longer) are abandoned.

`run_enter` ordering: `Resolved::from_intent` runs **first**, then the
`needs_golden`/`needs_confirmed` candle gate. So a false-golden tiny pinbar
(signal_high ≈ signal_low → degenerate bracket) fails *resolve* before the
golden gate is even reached → the `resolve-failed` we saw, not `needs-golden`.

## Fix (engine FSM, pure)

The fix belongs in the pure FSM because that's where the premature `Done`
happens, and `Resolved::from_intent` is itself pure (intent + shell + pip_size,
all available to the FSM).

For a **`PinePattern` (single-shot) enter only** — M/W heartbeat is untouched,
its `NotArmedYet` decline is by-design and owned by `run_enter`:

- [x] **A.** Before firing, pre-resolve the bracket via `Resolved::from_intent`
      on the signal-folded shell. If it **fails**, do not fire / latch / go
      `Done` — stay in `AwaitEntry`. Vetos keep ticking; a later bar can
      re-form a valid pattern.
- [x] **B1.** Apply the `needs_golden` / `needs_confirmed` gate against the
      latched signal's flags before firing, so a non-golden bar declines
      cleanly in the FSM (consistent with the intent's declared gates) instead
      of relying on a downstream gate that resolve pre-empts.

## Tests (engine crate, native)

- [x] Pine enter whose geometry resolves → fires + `Done` (unchanged path).
- [x] Pine enter with a degenerate (signal_high ≈ signal_low) bar → does NOT
      fire, phase stays `AwaitEntry`. (Finding A regression fixture.)
- [x] After a resolve-failed Pine bar, a subsequent veto-level cross still
      fires the veto (plan not abandoned). (Acceptance criterion 3.)
- [x] Pine enter with `needs_golden` on a non-golden latched bar → no fire,
      phase stays `AwaitEntry`. (Finding B1.)
- [x] M/W heartbeat enter is unaffected — still fires every bar, never
      pre-resolved in the FSM.

## Done checklist
- [x] cargo test (engine 51, core 559, worker lib 227, workspace) green
- [x] cargo clippy clean (workspace)
- [x] cargo fmt
- [x] CHANGELOG (v53) + README sync
- [ ] commit + push + tag v53 + advance parent pointer
