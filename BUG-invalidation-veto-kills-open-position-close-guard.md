# Bug: a terminal invalidation veto retires the plan, killing the per-position reversal-close guard while the position is still open

**Severity:** Medium-High — a trade that ran ~80% to TP then reversed off
resistance round-trips to break-even/SL instead of closing for a partial win,
even though the `07-close-on-sr-reversal` guard exists and would fire.
**Component:** `engine/src/evaluate.rs` (shared engine, so live worker + replay).
**Found via:** XAU_XAG H1 H&S short, 2026-07-21, `tv-arm-staging --strategy-v2
--qm-entry=market replay`.

**Status: ✅ FIXED (`38cc884`) via approach 1, the engine latch.** This header read
"DIAGNOSED, fix not yet implemented — awaiting approach sign-off" until 2026-07-28;
the sign-off happened and the fix landed, but the header was never updated. The
analysis below is retained because it is accurate and explains *why* the latch is
shaped the way it is.

**Verified on the original trade, not just in unit tests.** The
`xau-xag-close-on-reversal` replay fixture is this exact XAU_XAG H1 short:

```
2026-07-21 19:00  Veto (01-veto-too-low)          — plan SURVIVES (entries_blocked)
2026-07-21 22:00  entry #1 CLOSED ON REVERSAL → 68.8520   (R: +1.14)
2026-07-21 22:00  close-on-reversal (07-close-on-sr-reversal) — flattens the position
```

The +1.14R partial win this bug was losing is now booked. Two engine regression
tests pin it — `reversal_close_still_fires_on_a_later_bar_after_a_stop_next_entry_veto`
and `same_bar_stop_next_entry_veto_does_not_shadow_a_per_position_close` — and both
were mutation-verified on 2026-07-28: restoring the old
`terminal_fired = true` for a non-retiring veto fails exactly those two.

**The live worker needed no change**, despite the "must change together" list below
naming `persist_plan_state`. It keys on `eval.done`, and the engine fix means a
`StopNextEntry` invalidation no longer sets `Phase::Done` — so the plan is simply
never cleared. The engine change reaches the worker through `eval.done`; there is
no separate worker-side latch to maintain, and adding one would be the divergence
risk, not the fix.

## Summary

Trade: SHORT entry 21 Jul 12:00 @ 70.146, SL 71.282, TP 68.324. On the chart a
golden reversal off resistance near TP should have fired
`07-close-on-sr-reversal` and flattened the position for a partial win. It never
did — the trade rode to its break-even stop 2 days later.

## Confirmed root cause (with real data)

Rebuilt the exact plan via `tv-arm-staging ... plan-out`. It **correctly**
carries `07-close-on-sr-reversal` with `sr_bands = [[68.6353, 68.7727] (drawn
S/R), [68.32368, 68.392] (auto TP-resistance band, far edge = TP 68.324)]`,
`needs_golden = true`. **tv-arm built the plan right — the plan-build is NOT the
bug.**

Stripping the three terminal vetos (`01-veto-too-high`, `01-veto-too-low`,
`02-veto-trade-expiry`) from the plan JSON and re-running proves the close
**fires**:

```
bar 2026-07-21 22:00 +10:00  ◆ GOLDEN Long Pinbar (size 0.397 atr 0.379)
    → close-on-reversal (07-close-on-sr-reversal) — flattens the open position  (close 68.852)
```

(A golden long reversal off the drawn S/R band; the pinbar's wick-50% anchor
lands in `[68.6353, 68.7727]`.)

But in the real run `01-veto-too-low` fires at **19:00** — 3 hours **before** the
22:00 reversal — and sets `Phase::Done`:

- **Live (cron):** `persist_plan_state` (`trade-control-cron/src/engine.rs:421`)
  archives + `clear_plan_state` + `clear_trade_plan` — the plan is deleted, so no
  later tick can fire the close.
- **Replay:** the driver loop `break`s on `eval.done`
  (`cli/src/bin/replay_candles/replay.rs`), stopping all further evaluation.

The per-position close guard (`RuleKind::PerPositionClose`) is armed
**AwaitEntry-only** (`armed_in_rule`, `engine/src/evaluate.rs:2598-2605`), so once
`Phase::Done` it's dead.

`too-low` for a SHORT is **pcl-exhausted** (price ran ~80% to TP) — a
`StopNextEntry` veto that correctly does NOT close the position (see
`BUG-too-low-closes-positions.md`). But it retires the whole plan, and
pcl-exhausted fires at ~80%-to-TP — exactly where the TP-resistance band sits —
so the two collide by construction on every H&S trade.

## The conflation

A `StopNextEntry` invalidation fuses two effects into one `Phase::Done`:
(a) stop future entries [correct]; (b) retire the plan entirely [wrong while a
position is open and a `PerPositionClose` guard exists].

## Fix plan (engine-internal latch — NOT a broker query)

Prior-art hard constraint (`reversal_close_spine_retire_recurring` memory / v66):
the engine must stay position-agnostic — the `positions` param on
`evaluate_plan` and `PositionView`/`OpenSet` were deliberately removed. So:

- New `PlanState` bool `entries_invalidated`. A `StopNextEntry`
  `SetupInvalidation` that fires *while the plan carries a `PerPositionClose`
  guard* sets this latch **instead of** `Phase::Done`.
- Latch blocks `evaluate_entry` + the break/retest spine, but the guard scan
  keeps running. `armed_in_rule(PerPositionClose)` arms on `AwaitEntry` **or**
  `entries_invalidated`.
- The plan truly retires (`Done`) only on a `ClosePositions`-level veto,
  `trade-expiry`, or `not_after` (which bounds the lingering).

**Must change together or the replay oracle silently diverges:**
- **engine (shared):** `core/src/plan_state.rs` (latch + `advanced_vs`),
  `engine/src/evaluate.rs` (don't break/Done on a StopNextEntry-invalidation when
  a close guard exists; gate spine+entry on the latch; `armed_in_rule`;
  `evaluate_guards` terminal handling).
- **live worker:** `trade-control-cron/src/engine.rs::persist_plan_state` (don't
  clear a latched-but-not-retired plan).
- **replay:** `cli/src/bin/replay_candles/replay.rs` — `if eval.done { break }`
  must key off TRUE retirement, not invalidation, so the later Close fire lands
  in `fires` → `collect_close_fires_from` → `apply_reversal_close` (the forward
  sim is a frozen `candles[i..]` slice with no plan-rule access — it can't
  produce the Close itself).

Bound the resident plan by `trade-expiry`/`not_after` so a flat plan (SL already
hit) doesn't tick a dead guard forever. `run_close` is a no-op when flat, so this
is tidiness, not correctness.

## Open decision (blocking implementation)

Three approaches (see the AskUserQuestion in-session):
1. **Engine-latch fix** (above) — recommended, keeps replay==live inside the
   shared FSM.
2. **Gate keep-alive on an actually-open position** — needs broker state in the
   engine (v66 removed it); splits engine/replay; higher divergence risk. Not
   recommended.
3. **Rethink the trigger** — in the TP zone, let the close guard outrank /
   suppress pcl-exhausted rather than touching plan-retire. Possibly simpler;
   different design.
