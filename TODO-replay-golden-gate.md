# TODO: replay golden-gate not enforced (BUG-replay-golden-gate-not-enforced)

## Root cause
- **Replay (real defect):** `resolve_fire_any` (the `--annotate` boxes + the
  four-way entry-style net-R chart) called `simulate_fill` WITHOUT first running
  the shared `entry_gate_block`. `render_fire` already gated; this path did not.
  So a `needs_golden` enter whose signal-folded shell carries `golden != Some(true)`
  (a stop/limit enter has `signal: None` → `golden: None`) was FILLED and tallied
  an R the live worker would have 412'd before placing. `grep golden` == 0 on that
  path because nothing logged golden there.
- **Arm (already-fixed-in-source, stale binary):** `--skip-golden` already maps to
  `needs_golden: !args.skip_golden` on both `build_trade_spec` and
  `build_mw_trade_spec`, and threads onto every enter intent. The bug report's
  `needs_golden: True` came from a `tv-arm-staging` binary built before that change.
  `cargo install` rebuilds it; a regression test now guards it.

## Done
- [x] `resolve_fire_any` runs `entry_gate_block` before `simulate_fill`; a block →
      new not-taken `FillKind::GateBlocked` (0R, `is_taken() == false`).
- [x] Greppable tracing: `golden: blocked @ <bar>` / `golden: ok @ <bar>`.
- [x] `FillKind::GateBlocked` handled in `annotate.rs` outcome_label ("gate-blocked").
- [x] Replay regression tests: needs_golden + golden:None stop enter → GateBlocked
      (not taken); same enter without needs_golden → taken (proves gate, not price).
- [x] Arm regression tests: `--skip-golden` clears `needs_golden` on HS + MW spec;
      default keeps it on.
- [x] `cargo test --workspace` green, clippy clean, fmt run.

## Notes
- Golden gate definition/test is SHARED via `core::candle_gate` /
  `core::allow_entry_gate` (unchanged) — worker and replay can't diverge.
- H&S `05-enter` is a `Trigger::PinePattern`, so its fire already carries a latched
  signal (golden rides the shell) and was gated. The hole was specifically the
  annotation/R path's missing gate call, which affected every entry-style chart.
