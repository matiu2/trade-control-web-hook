# TODO: replay golden-gate not enforced (BUG-replay-golden-gate-not-enforced)

## Root cause
- **Replay (real defect):** `resolve_fire_any` (the `--annotate` boxes + the
  four-way entry-style net-R chart) called `simulate_fill` WITHOUT first running
  the shared `entry_gate_block`. `render_fire` already gated; this path did not.
  So a `needs_golden` enter whose signal-folded shell carries `golden != Some(true)`
  (a stop/limit enter has `signal: None` → `golden: None`) was FILLED and tallied
  an R the live worker would have 412'd before placing. `grep golden` == 0 on that
  path because nothing logged golden there.
- **Arm (NO leak — the report's `rules[4]` is the close guard, not an enter):**
  `--skip-golden` maps to `needs_golden: !args.skip_golden` on `build_trade_spec` /
  `build_mw_trade_spec`, threads onto the spec, and `assemble_trade` →
  `build_enter_alert` sets `intent.needs_golden = spec.needs_golden` on EVERY emitted
  enter (BCR `05-enter`, QM `09-enter-qm`). An end-to-end test now builds the actual
  emitted plan JSON and confirms every ENTER rule is `false` under `--skip-golden`.
  The coordinator's `rules[4].intent.needs_golden = true` is the **06-close-on-reversal**
  guard: in a raw-style plan the rule order is
  `[0]01-veto [1]pcl-veto [2]02-trade-expiry [3]05-enter [4]06-close-on-reversal`,
  and the close guard hardcodes `needs_golden: true` BY DESIGN (a reversal close
  needs a golden candle). `--skip-golden` governs the ENTRY gate only; it does not
  and should not clear the CLOSE gate. So there is nothing to fix on the arm side —
  the earlier "stale binary" guess was wrong, and so is the "arm leak" reading.

## Done
- [x] `resolve_fire_any` runs `entry_gate_block` before `simulate_fill`; a block →
      new not-taken `FillKind::GateBlocked` (0R, `is_taken() == false`).
- [x] Greppable tracing: `golden: blocked @ <bar>` / `golden: ok @ <bar>`.
- [x] `FillKind::GateBlocked` handled in `annotate.rs` outcome_label ("gate-blocked").
- [x] Replay regression tests: needs_golden + golden:None stop enter → GateBlocked
      (not taken); same enter without needs_golden → taken (proves gate, not price).
- [x] Arm regression tests (spec builder): `--skip-golden` clears `needs_golden` on
      HS + MW spec; default keeps it on.
- [x] Arm regression tests (EMITTED PLAN JSON, end-to-end): every ENTER rule honours
      `--skip-golden` (false) / default (true), incl. strategy-v2's two enters. This
      is the path the spec-only test missed; it confirms there is no downstream leak.
- [x] `cargo test --workspace` green, clippy clean, fmt run.

## Notes
- Golden gate definition/test is SHARED via `core::candle_gate` /
  `core::allow_entry_gate` (unchanged) — worker and replay can't diverge.
- H&S `05-enter` is a `Trigger::PinePattern`, so its fire already carries a latched
  signal (golden rides the shell) and was gated. The hole was specifically the
  annotation/R path's missing gate call, which affected every entry-style chart.
