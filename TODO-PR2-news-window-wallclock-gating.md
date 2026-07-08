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

### 3. Worker: fire controls on candle-less 5s ticks  [x]
- [x] `trade-control-cron/src/engine.rs`: replaced the bare `fresh.is_empty()`
      early-return with `tick_controls_only(...)`: runs `evaluate_controls_only`
      against `now` + last-known closed bar (kept from the fetch before
      `filter_new_candles`; synthesized flat bar at watermark if the fetch was
      empty), and if anything fired persists + dispatches via the SAME
      persist/dispatch/bundle path (empty candle windows, never `done`, shadow
      honoured). No control fire → the old cheap no-op.
- [x] No double-fire: `state.fired` latch dedupes; watermark not advanced (a
      control tick processes no bar), so the real bar still ticks next.
- [x] No detector-window fetch on the control path (controls are pure TimeReached).

### 4. Replay: inject virtual boundary ticks  [x]
- [x] `cli/src/bin/replay_candles/replay.rs`: `inject_control_ticks(...)` runs
      before each bar's `evaluate_plan`, replaying every unfired control epoch in
      `(prev_close, this_close)` (strict, so a bar-close epoch isn't double-fired)
      via `evaluate_controls_only` at the epoch — the SAME engine entry the worker
      calls. Pins the virtual clock, applies pause/resume through the same
      `pause_gate` helpers, records the fires. `control_epochs_between` selects them.
- [x] Bar ticks unchanged; virtual ticks only add control fires at exact epochs.

### 5. Parity test  [x]
- [x] `virtual_tick_opens_window_at_same_instant_as_worker_controls_only`: drives
      both the worker path (`evaluate_controls_only`) and the replay path
      (`control_epochs_between` + `inject_control_ticks`) at a 10.5h sub-bar epoch
      and asserts they fire the SAME rule.
- [x] `sub_bar_pause_epoch_opens_via_virtual_tick_and_suppresses_enter`: an
      end-to-end `run(...)` where a sub-bar pause opens via the virtual tick and
      suppresses a later enter. 75 replay tests pass; clippy/fmt clean.

### 6. Wrap up  [ ]
- [x] cargo test / clippy / fmt (engine 122, cron 21, cli 257+75; worker builds).
- [x] README (wall-clock control note) + CHANGELOG (v70).
- [ ] Merge to main, tag v70, advance parent pointer. Deploy to dev + staging.

## Parity invariant (do not break)
Worker and replay MUST open/close each window at the same wall-clock instant.
The engine boundary predicate is the single source of truth; both drivers only
differ in HOW they present `now` (worker: real 5s ticks; replay: virtual ticks).
