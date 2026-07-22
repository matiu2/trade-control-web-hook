# TODO — same-bar terminal-veto vs per-position-close ordering (replay↔live parity)  ✅ DONE

## Problem
Replay `evaluate_guards` iterates `plan.rules` in order. When a terminal
invalidation veto (`too-low`, `stop-next-entry`) and a non-terminal
`07-close-on-sr-reversal` both fire on the same bar, the veto (lower rule
index) hit `state.phase = Done; return`, abandoning the loop before the close
was reached. The position rode to SL instead of being flattened at the
reversal. USD/ZAR M15 2026-07-20: too-low @ 16.44519 and the golden long
reversal-close both fired on the 11:30Z bar; live flattens ~BE, replay
reported −1R.

Live has no such collision: the two alerts arrive as independent requests.
`too-low` (stop-next-entry) blocks future entries; the separate close alert
calls `run_close → close_positions` and flattens regardless.

## Fix (replay engine only — live is already correct)
`engine/src/evaluate.rs::evaluate_guards`: record a pending terminal instead
of early-returning; keep scanning so a same-bar `PerPositionClose` still
dispatches (phase left unchanged during the scan so the close stays armed);
apply `Phase::Done` after the loop.

## Steps
- [x] `evaluate_guards`: pending `terminal_fired` flag; Done applied post-loop.
- [x] Regression test `same_bar_terminal_veto_does_not_shadow_a_per_position_close`
      (too-low + close same bar → both fire, close dispatched, then Done).
      Verified it FAILS without the fix (`got ["01-veto-too-low"]`) and passes with.
- [x] Existing guard tests still green (174 lib + 3 integration).
- [x] clippy (no new warnings in edited region) + fmt.

## Live needs no matching change (verified)
`core/dispatch/veto.rs:128`: a `stop-next-entry` veto returns true WITHOUT
touching the position; only `close-positions`-level vetos call
`close_positions`. The reversal-close (`run_close`) flattens independently.
Replay was under-modeling live, not a strategy-rule change → replay-engine-only.

## Root cause was TWO bugs (both replay-only)
1. **Guard latched before applying `needs_golden`** (`eval_pine_guard`): a
   NON-golden reversal latched the one-shot close guard and shadowed a later
   golden reversal. Primary fix — candle-quality gate now runs before the latch.
2. **Terminal veto shadowed a same-bar close** (`evaluate_guards`): terminal
   `return` abandoned the loop before a same-bar non-terminal close. Secondary
   fix — defer Done to after the loop.
Both needed for the USD/ZAR repro (golden close fires on the SAME bar as the
terminal too-low veto). End-to-end: replay flips −1.00R → +0.36R, matching live.

## Replay fixtures
- gbpaud-expiry: unaffected (✅ --check).
- sgdjpy-spread-floor: unaffected (✅ --check) — untracked, lives in main checkout.
- uk100-qm-v2: was pinning the bug (2 needs_golden closes on non-golden
  reversals that live rejects). Reblessed to the live-faithful outcome;
  rationale in its meta.json `rebless_note`. Net R unchanged (+0.00).

## Ship
- [x] engine fix + 3 unit tests (all verified fail-without-fix)
- [x] full suite green (engine 176+3, cli 267+109+22, core 894); clippy 0 err; fmt clean
- [x] uk100 fixture reblessed + rationale in meta.json
- [x] CHANGELOG v113
- [ ] commit + push branch
- [ ] merge to main + staging, advance parent submodule pointer
- [ ] memory note
