# TODO — replay-candles automatic warm-up prefix

## Problem
`replay-candles` arms the plan at window-start (`SEED_BARS=10` seed, then
everything fires). Two failures fall out:

1. **ATR/detector not warmed.** The candle detector's golden test needs a
   warmed ATR (96 bars on 15m). A short window leaves `atr=None` → nothing
   can ever be golden → a `needs_golden` enter never fires. (NZD/CHF
   2026-06-19: the 1pm short pinbar *is* golden once ATR is warm —
   range 0.00025 ≥ ATR(96) 0.000247 — but the replay reported `atr=None`.)
2. **Veto levels touched during warm-up retire the plan.** Every veto guard
   is terminal. If we just prepend warm-up candles to the live loop, an
   earlier touch of too-low/too-high (e.g. NZD/CHF too-low on 16 Jun) fires
   the veto and ends the plan before the real entry.

So the warm-up must be a **silent prefix**: feed the detector history but
fire nothing until the operator's requested `--start`.

## Plan
Warm-up sized in **bars**, not days — a bar count scales across granularities
(96-bar ATR on 15m, but only ~18 bars in 3 days on 4h). Default **200 bars**.

- [x] Add `--warmup-bars <usize>` flag (default **200**) to `replay-candles`.
- [x] Pull `warmup_bars` extra bars before `start`: `pull_from = start -
      warmup_bars * bar_seconds` (a time offset; cache pull is time-windowed).
- [x] Pass the operator's `start` (the live boundary) into `replay::run`.
- [x] In `run`, set the seed boundary to the count of bars with
      `time < live_start` (so all pre-start bars seed without firing),
      floored at `SEED_BARS` and capped so ≥1 live bar remains.
- [x] Log the warm-up span + how many bars are warm-up vs live.
- [x] Tests: a plan whose veto level is touched only in the warm-up prefix
      does **not** fire that veto; the live entry still fires.
- [x] clippy + fmt. README: replay-candles section TODO (no dedicated section
      exists yet; flag is self-documenting via --help).

## Outcome (verified on NZD/CHF 2026-06-19)
With `--warmup-bars 200`, the 1pm short pinbar (03:00 UTC) now detects
`golden=true` (ATR warmed to 0.0002445, range 0.00025). So warm-up fixed the
golden problem. The enter still declines — but for a **legitimate** reason:
the bar's close == its low == signal_low (0.46332), so the short stop-entry at
`signal_low + 1 pip` (0.46342) sits ABOVE the close → `Resolved::from_intent`
returns `InvalidGeometry` → `pine_entry_dispatchable` declines this bar (per
bug #13, a decline-this-bar, plan stays armed). That's correct behaviour: a
"break below the low" sell-stop has no room when the bar closed on its low.
NOT a bug in the engine, offset sign, or warm-up.

## Not in scope
- The too-high cap drawn below the head (separate charting issue).
- ATR-length parity (14 vs 96): confirmed a red herring for this bar —
  both lengths mark it golden when warmed.
