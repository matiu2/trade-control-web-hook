# TODO — OnClose break-and-close: origin-side, not prev-close edge

## Problem
The OnClose directional cross (`level_crossed`, `engine/src/evaluate.rs`) is an
**edge detector**: `up = prev < upper && c >= upper`. It fires only on the bar
that makes the below→above transition. Two ways this loses a real break-and-close:

1. **Spread-hour skip then next-bar-already-above.** A spread-hour bar closes
   above the neckline (suppressed → not stamped), then the *next* bar
   opens+closes above too. `prev` (the suppressed bar's close) is already
   `>= upper`, so `prev < upper` is false → the clean bar never stamps. The
   break-and-close is lost forever. (Bit GBP/USD, EUR/GBP.)
2. **Armed already-above.** If the window's first processed bar is already
   closed above the line, no transition bar exists → never stamps.

## Operator's model (the fix)
The **origin side** is set by the **open of the first bar the plan processes**
(= the arm-time / replay-start bar; both live & replay have ~200 bars of context
but the arm bar is the reference). Thereafter **any bar whose CLOSE lands on the
opposite side past the far zone edge = break-and-close**, stamped once (latched).

Scope: **ALL OnClose consumers** (break-and-close, too-high/too-low invalidation,
M/W abort, drawn lines) — the same "already settled past = the event" logic is
correct for the caps too (a late-armed/ suppressed already-past close should
invalidate). User chose "all OnClose consumers".

## Must not break
- **Zone-of-the-line fix (NAS100 short 2026-07-02):** a close *inside* the buffer
  zone on the near side must not fire. Tests
  `on_close_zone_break_registers_after_near_side_dip`,
  `on_close_zone_close_inside_zone_does_not_fire`. (Both survive: a near-side dip
  doesn't reach the far edge.)
- **Seed bar:** its OPEN sets origin; its own CLOSE may fire if it closed to the
  opposite side. The old `on_close_cross_does_not_fire_on_seed_bar` /
  `horizontal_on_close_fires_when_close_crosses_prior_close` (2nd case) tests
  encode the OLD edge semantics and will be **updated** to origin semantics.

## Plan (PART 1 — DONE, all tests green)
- [x] Add `PlanState.origin_open: BTreeMap<String, f64>` (mirror `last_close`):
      the open price of the first bar each OnClose rule evaluated. Set once,
      never overwritten. `#[serde(default, skip_serializing_if = is_empty)]`.
- [x] `record_last_close` sibling: `record_origin_open` — insert the candle's
      OPEN under rule_id iff absent AND trigger is an OnClose cross.
- [x] `level_crossed` OnClose arm: DUAL-MODE via `OnCloseRefs { origin,
      prev_close, settled }`. **settled** (latching preps+vetos) fires on origin
      one side + close past the opposite far edge. **edge** (entry crosses) keeps
      the old prev-close transition detector — a multi-shot entry cross must not
      re-place every settled-past bar (caught by 3 v2 replay tests: 11 vs 2).
- [x] Thread refs from `fire_rule` — `settled = rule.intent.action != Enter`;
      records origin before the fire decision, before the spread-hour gate.
- [x] `retest_crossed` OnClose path: same plumbing, `settled: true` (a prep).
- [x] `advanced_vs()` in `plan_state.rs`: `origin_open` set-once, excluded from
      no-op detection (documented) + added to `seed()`.
- [x] Tests: `on_close_fires_when_bar_opened_and_closed_above_origin_below`,
      `on_close_break_fires_when_transition_bar_was_skipped` (stateful),
      `on_close_entry_cross_edge_fires_on_transition_only`, both zone tests green.
      `eval_trigger` made private (was pub, no external users).
- [x] cargo test (engine 152, core 852, cron 11, cli replay 93) / clippy / fmt.
      The one cli failure (`manifest_path_is_under_config_trade_control`) is a
      pre-existing $HOME/config-path parallelism flake — passes in isolation.
- [x] Replay ≡ worker: shared `seed_plan_state` seeds `origin_open` for both the
      cron seed and replay warm-up. No separate change.
- [x] CLAUDE.md OnClose note updated (was "unchanged").
- [ ] Deploy staging, verify a real plan (EUR/GBP ihs), merge to main, advance
      parent pointer.

### Migration note (existing persisted plans)
Existing `PlanState` rows lack `origin_open` (serde default = empty). Safe:
- A plan still in `AwaitBreakAndClose` sets origin from its next-processed bar's
  open — still on the origin side (it hasn't broken yet). Correct.
- A plan already past the b&c has the rule latched in `state.fired`, so
  `evaluate_break_and_close` returns early and `fire_rule` never runs for it.

## Part 2 (after part 1 green): buffer param + time decay
- [ ] `tv-arm --cross-buffer-pct` flag (mirror `--retest-atr-step`) threading to
      `TradePlan.cross_buffer_pct` in `trade_plan_build.rs:121`.
- [ ] Investigate the buffer time-decay the operator recalls (buffer grows over
      time). Confirm whether that exists / is desired — may be conflating with
      the retest `retest_atr_step` decay. Report before building.
