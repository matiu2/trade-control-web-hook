# TODO: StopNextEntry veto must not retire the plan (keep position + close-guard alive)

**Rule (operator, 2026-07-23):** a `too-low`-style veto
(`VetoLevel::StopNextEntry`) must ONLY block future entries. It must NOT touch
the open position, and must NOT stop the trade's monitoring — specifically it
must NOT kill the `07-close-on-sr-reversal` guard. Only a genuinely terminal end
retires the plan: a `CancelPending`/`ClosePositions` veto, `trade-expiry`, or
`not_after`.

**Bug:** today ALL `Action::Veto`/`Invalidate` guards are `SetupInvalidation` →
terminal → `Phase::Done` → plan archived/cleared → per-position close guard
(armed AwaitEntry-only) dies. A later golden reversal off the TP-resistance band
never fires. Confirmed XAU_XAG H1 short 2026-07-21 (close would fire 21 Jul
22:00; `too-low` retired the plan at 19:00). Affects live + replay (cron-engine
fires the close; not a standalone alert). See
`BUG-invalidation-veto-kills-open-position-close-guard.md`. This GENERALIZES the
v113 same-bar fix to the later-bar case.

**Veto levels (from cli/src/trade_patterns.rs):**
- `too-high` (invalidation, thesis dead) = `ClosePositions` → terminal (keep).
- `too-low` (pcl-exhausted) = `StopNextEntry` → entries-only (the fix).
- `trade-expiry`, M/W abort/overshoot = `CancelPending` → terminal (keep).

## Design (engine-latch keyed on VetoLevel — no broker in engine, per v66)

New `PlanState.entries_blocked: bool`. A guard whose veto level is
`StopNextEntry` fires+latches but sets `entries_blocked = true` instead of
`Phase::Done`. `entries_blocked` gates entry + break/retest spine (no new
entries, setup frozen). `armed_in_rule(PerPositionClose)` arms on `AwaitEntry`
OR `entries_blocked`. Plan retires only on a terminal guard / expiry / not_after.
Replay's `if eval.done { break }` already keys off `eval.done`, which stays false
while only entries_blocked — so it keeps evaluating and the later close fire
lands (replay never wrote fired vetos to its store, so no KV path needed).

## Steps
- [x] 1. `core/src/plan_state.rs`: `entries_blocked: bool` (serde default),
      in `advanced_vs` + `seed`. Test `entries_blocked_defaults_false_and_is_an_advance`.
- [x] 2. `engine/src/evaluate.rs`: `veto_retires_plan(rule)` — StopNextEntry
      level → false (entry-block only); else terminal. `PREP_STEP` unaffected.
- [x] 3. `evaluate_guards`: a StopNextEntry invalidation sets
      `state.entries_blocked = true` (latch), NOT `terminal_fired`. Same-bar
      non-terminal close still dispatches (v113 preserved).
- [x] 4. Spine-freeze: `if state.entries_blocked { continue; }` before the phase
      match (no new entries, spine frozen, phase unchanged = not retired).
      `armed_in_rule(rule, phase, entries_blocked)` arms PerPositionClose when
      `entries_blocked`.
- [x] 5. Regression tests (both fail-without-fix verified):
      `same_bar_stop_next_entry_veto_does_not_shadow_a_per_position_close`
      (renamed from the v113 test; now asserts NOT done + entries_blocked),
      `reversal_close_still_fires_on_a_later_bar_after_a_stop_next_entry_veto`
      (2-tick end-to-end). Updated 5 tests that meant "terminal" to use the new
      `terminal_veto_rule` helper (ClosePositions level).
- [x] 6. Live reproduction ✓ — XAU_XAG replay now: `19:00 Veto too-low
      phase=AwaitEntry (not Done)` then `22:00 close-on-reversal flattens
      (close=68.852)`. Was: retired at 19:00, rode to BE.
- [x] 7. engine 180+3, core 895, cli 267+108 green (uk100 fixture failure is
      PRE-EXISTING on clean HEAD, unrelated). gbpaud fixture plan.json:
      trade-expiry level→close-positions (matches tv-arm today); expiry replay
      test given explicit level. clippy clean; fmt done.
- [ ] 8. CHANGELOG vNN; commit+push; merge to staging + redeploy; advance parent
      submodule pointer; memory note.

## Pre-existing unrelated failure (NOT mine)
`uk100-qm-v2-confirmation-fixed` fixture diverges (extra `09-enter-qm` fire) on
CLEAN HEAD with all my changes stashed — a stale fixture from a prior change.
Left as-is; flag separately.
