# PR2 — sub-bar (wall-clock) news/blackout window gating

**Branch:** `feat/news-window-wallclock-gating` (off main @ v69)
**Worktree:** `../trade-control-web-hook-news-intrabar` (sibling of repo — path-deps resolve)

## Problem

Control `TimeReached` rules (pause/resume/news-start/news-end) fire per **closed
candle** via `eval_trigger`: `candle.time.timestamp() >= at_epoch`
(`engine/src/evaluate.rs:1334`). A 14:30 event on an H1 chart only opens when the
15:00 bar closes — ~30 min late, and the resume/close is late too. tv-arm already
bakes the real event minute onto the epoch (PR1), so the precision is there; the
engine throws it away by testing against the bar's open timestamp.

Worker engine tick runs every **5s** (`engine_secs` default) but early-returns
when no new closed candle arrived (`trade-control-cron/src/engine.rs:154`
`if fresh.is_empty() { return Ok(()) }`), and control rules are only evaluated
inside the per-candle loop. So the 5s cadence does nothing between bar closes.

## Design (user-approved)

1. **Engine:** control `TimeReached` fires against wall-clock **`now`** (already a
   param of `evaluate_plan`, currently `_now`), not `candle.time`. Guards
   (trade-expiry etc.) stay candle-driven — scope the change to control rules only.
2. **Worker:** on a 5s tick with `fresh.is_empty()`, still run a **control-only**
   evaluation against `now` + the last-known candle, so a mid-bar window opens
   ~14:30:05 live. (Lift the early-return for the control path only; spine/guards
   still need a candle.)
3. **Replay:** replay owns a **virtual clock** (`store.set_clock(now)`), so it
   injects a **synthetic sub-bar tick** at each unfired control epoch that lands
   strictly between two bar closes, driving `evaluate_plan` at that instant with
   the last-known candle. Reproduces the worker's sub-bar open/close **exactly**,
   not approximately.

Shared pure boundary logic lives in the engine so worker + replay can't diverge
(`[[strategy_changes_in_both_replayer_and_worker]]`).

## Steps

### 1. Engine: control TimeReached vs wall-clock now  [x]
- [x] Added `control_rule_fires(rule, …, now)`: control `TimeReached` tests
      `now.timestamp() >= at_epoch`; non-time control triggers fall back to
      candle-based `fire_rule`. Guards/spine `TimeReached` unchanged.
- [x] `evaluate_plan`: `_now` → `now`, passed to `evaluate_controls`.
- [x] Tests: `control_window_fires_sub_bar_when_now_reaches_epoch_before_candle_time`
      (14:30 fires at now=14:30 vs 14:00 candle; not at now=14:29); rewrote
      `pause_and_resume_fire_when_wallclock_reaches_each_epoch_and_dont_refire`
      to drive `now` per tick. Added `run_at` test helper. 121 engine tests pass.

### 2. Engine: candle-less control tick entry point  [x]
- [x] Added `pub fn evaluate_controls_only(plan, prior, last_candle, now,
      expires_at) -> PlanEval`: runs ONLY control rules against `now` + the
      last-known candle, no new bar. Never touches phase/guards/spine; exported
      from engine lib.
- [x] Test `evaluate_controls_only_opens_and_closes_a_window_with_no_new_bar`
      (open 14:30 + close 15:15, both mid-bar, zero new candles; phase stays
      AwaitEntry). 122 engine tests pass.

### 3. Worker: fire controls on candle-less 5s ticks  [ ]
- [ ] `trade-control-cron/src/engine.rs`: when `fresh.is_empty()`, instead of
      bare `return Ok(())`, run the control-only eval against `now` + last-known
      candle; persist + dispatch any control fires. Keep the fast-path cheap
      (no detector window fetch when only controls run).
- [ ] Confirm no double-fire: `state.fired` latch already dedupes across ticks.

### 4. Replay: inject virtual boundary ticks  [ ]
- [ ] `cli/src/bin/replay_candles/replay.rs`: between bar `i` and `i+1`, collect
      unfired control epochs in `(close_i, close_{i+1})`, and for each (ascending)
      set virtual clock + run control-only eval with the last-known candle.
- [ ] Bar ticks unchanged; virtual ticks only add control fires at exact epochs.
- [ ] Replay report/trace shows the window opening at the epoch, not the bar.

### 5. Parity test  [ ]
- [ ] One scenario (mid-bar news window on H1) asserted to produce the SAME
      open/close fire instants via the worker-path eval and the replay-path eval.

### 6. Wrap up  [ ]
- [ ] cargo test / clippy / fmt (engine, cli, worker, cron).
- [ ] README + CHANGELOG (v70). Note the sub-bar semantics + replay virtual ticks.
- [ ] Merge to main, tag, advance parent pointer. Deploy to dev + staging.

## Parity invariant (do not break)
Worker and replay MUST open/close each window at the same wall-clock instant.
The engine boundary predicate is the single source of truth; both drivers only
differ in HOW they present `now` (worker: real 5s ticks; replay: virtual ticks).
