# BUG: break-and-close origin seeded from the last warm-up bar (off-by-one)

**Status:** FIXED (engine `seed_plan_state` no longer seeds `origin_open`).

## Symptom

XAU_USD iH&S long `ihs-xau-usd-2c1a9f2f` (staging, armed 2026-07-20 13:04Z /
23:04 Brisbane). The live worker went straight to `01-veto-too-high` and
archived without ever taking the trade — no break-and-close, no retest, no
entry — even though the setup was a genuine winner (+1.03R in replay).

The replay was **cursor-fragile**: `tv-arm-staging replay` at the operator's
intended cursor of **20 July 11 PM Brisbane (`--start 2026-07-20T23:00`)`
reproduced the miss (+0.00R, stuck in `AwaitBreakAndClose`), while the same
plan at `--start 2026-07-20T23:04:09` (the plan's `armed_at`) entered and took
+1.03R. A **4-minute** cursor shift flipped the outcome. Same plan JSON, same
instrument, same candles — the only diff in the built plans was `replay_start`.

## Root cause

`engine/src/evaluate.rs::seed_plan_state` seeded each `OnClose` rule's
`origin_open` from `candles.iter().max_by_key(|c| c.time)` — the **newest bar
in the warm-up back-window**, i.e. the bar *immediately before* the cursor, not
the first *live* (cursor) bar.

The origin is the side of the neckline the plan **starts** on. Under origin-side
`OnClose` semantics, a break-and-close fires when a bar *closes on the far side
of the line from the origin*. If the origin is recorded already on the far
side, the break can never fire.

For this setup the descending neckline sat at ~4025.18 at 07-20 22:00 Brisbane.
The candles (OANDA H1 mid):

| bar (Brisbane) | open | high | low | close | neckline | origin side |
|---|---|---|---|---|---|---|
| 07-20 22:00 | **4025.325** | 4025.325 | 4006.505 | 4008.165 | 4025.18 | **ABOVE** |
| 07-20 23:00 | 4008.180 | 4017.480 | 4005.770 | 4005.825 | 4025.04 | below |
| ... | | | | | | |
| 07-21 10:00 | — | — | — | **4024.8** | 4023.52 | first close **above** → the real break |

The 07-20 22:00 bar **gapped open at its high** (open == high == 4025.325) and
then collapsed to 4008 — a whip-saw bar whose *open* momentarily sat above the
line. At cursor 23:00, that 22:00 bar is the newest warm-up bar, so
`origin_open` was recorded as 4025.325 (**above**). The plan was thereby treated
as "already broken above" and the break-and-close never re-fired → stuck in
`AwaitBreakAndClose` → died on `too-high`.

The **live worker** hit the identical bug: its persisted state showed
`origin_open: { 03-prep-break-and-close: 4025.325 }` — proving it, too, anchored
origin to the 22:00 warm-up bar and correctly-by-its-own-broken-logic never
stamped the break. So the missed live trade was **not** a real invalidation.

At cursor 23:04 the warm-up/live boundary shifts by one bar so the poisoned
22:00 bar is no longer newest — origin lands on a below-side bar and the break
fires. Pure luck, not correctness.

This is a variant of `break_close_edge_detector_misses_already_above`, but the
"already above" is a **false origin** from an off-by-one plus a whip-saw open,
not a real market condition.

## Fix

`seed_plan_state` no longer records `origin_open`. It still seeds `watermark`
and `last_close` from the newest warm-up bar (needed so the first live tick can
detect a cross without back-firing). The origin is instead recorded from the
**first live bar** during `evaluate_plan`, via the existing set-once
`record_origin_open` calls in `fire_rule` / `stamp_retest` — matching the
already-documented intent at `fire_rule` ("the seed bar is not special: the
first bar a rule sees records its own origin and can itself fire").

Replay and the live worker share `seed_plan_state`, so the fix lands in both
(replay == live invariant).

## Verification

- New unit test `seed_does_not_anchor_origin_to_the_warmup_bar`
  (`engine/src/evaluate.rs`): a warm-up bar that opens above a flat neckline no
  longer poisons the origin; the first live bar (below) fixes origin, and a
  later close above fires the break.
- Full engine suite green (181 + 3).
- Replay of `ihs-xau-usd-2c1a9f2f` at **both** `--start 23:00` and
  `--start 23:04:09` now converge to **+1.03R** (break-and-close stamps
  07-21 10:00, retest, entry fills, TP). Cursor-fragility gone.

## Follow-up (not done here)

- Consider making the origin robust to whip-saw opens (e.g. anchor to the
  cursor bar's open explicitly rather than "first bar the rule evaluates"), but
  with this fix the first live bar IS the cursor bar, so the concrete bug is
  closed.
- The live plan that already died is archived; this fix only affects
  future arms. No re-arm of the historical plan is attempted.
