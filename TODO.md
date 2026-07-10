# TODO — fix QM multi-shot re-entry (confirmed leg) — DONE (v79)

## Bug (fixed)
strategy-v2 QM enter (`09-enter-qm`, `needs_confirmed`, multi-shot) fired ONCE
on the first confirmed signal and never re-entered; `first_confirmed_signal_at`
was frozen on the first winner. Fixed by a per-plan re-entry watermark.

## Steps — all done
- [x] core/src/plan_state.rs: `last_confirmed_enter_at` (serde-skip None;
      advanced_vs; seed). Round-trip + advance + elided tests.
- [x] core/src/signals/state_machine.rs: `first_confirmed_signal_at` exclusive
      `after` bound; `LatchedSignal.signal_bar_time` (print bar N). Test:
      `after_watermark_advances_to_the_next_confirmed_short`.
- [x] engine/src/evaluate.rs: fold `last_confirmed_enter_at` into confirmed-first
      scan; stamp on confirmed multi-shot fire. Tests: re-fire + single-shot-once.
- [x] cargo test (core 813 / engine 132 / cli 356), clippy, fmt — green.
- [x] Verified vs operator's replay: entry #1 → SL, 8pm skipped, entry #2 → TP,
      3rd fire blocked by open-position backstop. Net +0.54R (was −1.00R).
- [x] README (09-enter-qm row) + CHANGELOG v79.
- [ ] merge to main + advance parent gitlink + tag v79 (pending review/push).
