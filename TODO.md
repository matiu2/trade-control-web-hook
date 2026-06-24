# TODO — window-aware single-slot role selection in replay mode

## Problem

`replay-candles` on an old-trade replay emitted a spurious extrapolation
warning ("anchor … is outside the N-bar window") AND produced no entries.
Root cause: `tv-arm`'s `classify()` resolves single-slot roles (invalidation,
neckline, retest, tp_fib, trade_expiry) via latest-wins, picking the drawing
with the *newest* anchor time. In TV replay mode the chart carries both the
historical pattern's drawings and recent (live-dated) ones; latest-wins grabs
the recent drawing, so its anchor lands outside the replayed window →
break-and-close never evaluates → no entry → the warning.

## Decision

- Mode signal: reuse `args.register_plan` (same signal that already drives
  `BuildStrictness` at pipeline.rs). `--plan-out` *without* `--register-plan` =
  offline / replay build → **window-aware** selection. `--register-plan` (with
  or without `--plan-out`) = live arming → **latest-wins** (unchanged).
  register-plan is KING when both flags are present.
- Apply window-aware preference to **all single-slot roles**: invalidation
  (too-high / too-low), break_and_close, retest, tp_fib, trade_expiry.
- Multi-slot roles (blackout/news pairs, sr_levels, mw_path, position,
  prep_expiries) keep their current logic.

## Window-aware selection (replay mode), per single-slot role

1. Prefer drawings **fully inside the visible range** — among those,
   latest-wins.
2. Else the one anchored **before & closest** to the visible-range start
   (largest `latest_time()` ≤ `view.from`).
3. Else plain latest-wins (never select nothing → no silent regression).

## Tasks — DONE

- [x] Add `SlotPref { LatestWins, WindowAware((i64,i64)) }` to roles.rs.
- [x] `pick_slot` / `pick_slot_with_label` take the pref + share
      `pick_window_aware`; replaced `latest_only` / `latest_with_label`.
- [x] `classify()` takes the pref; threaded to every single-slot resolution
      (invalidation, neckline, retest, tp_fib, trade_expiry). mw_path stays
      LatestWins (already in-window-filtered).
- [x] Threaded the pref from both `classify()` call sites in pipeline.rs
      (WindowAware(view) when `!args.register_plan`, else LatestWins; register
      is king when both flags set).
- [x] Updated test call sites (LatestWins so existing assertions hold).
- [x] New tests:
  - [x] replay mode prefers in-window neckline over newer out-of-window one
  - [x] before-and-closest fallback when none in window
  - [x] last-resort latest when all drawings to the right of the window
  - [x] live mode (LatestWins) still picks newest even when out of window
  - [x] window-aware also applies to invalidation + trade_expiry
- [x] cargo test -p tv-arm (147 pass), workspace tests green, clippy
      -D warnings clean, fmt clean.
- [x] wasm worker lib build clean (tv-arm not a worker dep — cannot affect it).
- [ ] Commit + push; merge to main (deploy dev) + staging (deploy staging).
