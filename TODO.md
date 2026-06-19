# Fix: confirmation fires early instead of waiting full window

## Bug

Desired: signal candle closes (bar 0), wait `confirm_bars` (=2) more bar closes,
and ONLY at the end of the window check whether price broke through the
extreme during the window. If it broke at any point in the window → confirm.

Actual (both Pine v2.5 and Rust port): the moment a bar within the window
breaks the extreme it confirms EARLY (`bars_elapsed <= confirm_bars && pushed`).

Semantics chosen by user: "at end of window if ever broke" — latch the break,
but only transition to VALID / fire the alert when `bars_elapsed == confirm_bars`.

## Plan

- [x] Pine `candle-signals-v2.pine`: per-direction break-latch + confirm only at
      `bars_elapsed == confirm_bars`. Bump to v2.6, update header docs.
- [x] Rust `core/src/signals/state_machine.rs`: add `broke` flag to `Tracked`,
      transition to Valid only at window end, keep invalidation rules.
- [x] Update / add Rust tests:
      - confirm does NOT fire early (bar 1 break, as_of=1 -> not confirmed/not fires)
      - confirm DOES fire at window end (as_of=2 -> confirmed/fires)
      - transient break in window still confirms at window end
      - no break in window -> invalid at window end (existing test adjusted)
- [x] cargo test, clippy, fmt green
- [x] Update README + pine parity memory if behaviour-visible
- [x] Commit + push both; advance parent pointer
