# TODO: fix spread-hour widen — dead 30-min lead on the replay bar-boundary clock

Bug: `BUG-spread-hour-widen-no-subhour-lead.md`.

## Root cause
`spread_hour_widen_for` (the pure seam behind `spread_hour_widen_frac`) hardcoded
a `SPREAD_HOUR_LEAD_MINUTES = 30` lead. On an H1 bar-boundary `now` (minute 0),
`60 - 0 = 60 > 30`, so the look-ahead into the next flagged hour was unreachable.
The offline replay only evaluates at bar closes (:00 on H1) → never pre-armed the
widen → the widen and the stop-out collided on the same 21:00Z spike bar. The
live worker ticks every 15 min so it *did* reach the :30–:59 lead window → replay
↔ live diverged on the GBP/AUD single-hour [21] case.

## Fix (Fix 1 + Fix 2 unified in the shared `core` seam)
Parameterized the lead. The replay passes a lead ≥ its bar length (a full bar);
the live cron keeps the 30-min default.

- [x] core: `spread_hour_widen_for` gains a `lead_minutes` arg. New public
      `spread_hour_widen_frac_with_lead(instrument, now, lead_minutes)`;
      `spread_hour_widen_frac` delegates with `SPREAD_HOUR_LEAD_MINUTES` (live
      behaviour byte-identical). `is_spread_hour` / `mask_active_with_lead`
      (suppression + pending-lifecycle) untouched — still 30-min.
      Tests: `widen_30min_lead_is_dead_at_a_bar_boundary`,
      `widen_full_bar_lead_pre_arms_on_the_prior_bar`,
      `widen_frac_with_lead_matches_the_default_helper`.
- [x] engine: `widened_stop_at` derives `bar_minutes` from `bar_seconds_of` and
      passes `lead = SPREAD_HOUR_LEAD_MINUTES.max(bar_minutes)`.
      Test: `widened_stop_at_pre_arms_a_full_bar_before_the_spread_hour_on_h1`
      (GBP/AUD [21] mask, H1 grid → widen lands on the 20:00 bar, not 21:00).
- [x] Live cron unchanged (still calls `spread_hour_widen_frac` = 30-min lead).

## Follow-up (2026-07-15): report the widen at its SUB-CANDLE instant (06:30)
Operator: the replay showed the widen at 06:00 (bar open), but the live cron
widens mid-bar at ~06:30 (30-min lead before the 07:00 spike). The full-bar-early
lead was the right *effect* but the wrong *displayed time*.
- [x] core: new `spread_hour_widen_instant(instrument, bar_open, bar_seconds)`
      returns the EXACT sub-candle widen moment + its widen fraction: the
      `T - SPREAD_HOUR_LEAD_MINUTES` lead instant when the bar leads into a flagged
      hour (20:30Z for a 20:00–21:00 bar), or `bar_open` when the bar's own hour is
      already flagged. Tests: `widen_instant_*` (5).
- [x] engine: `widened_stop_at` calls `spread_hour_widen_instant` and reports
      `SpreadWiden.at` at the sub-candle instant (06:30), not the bar open.
      Test renamed `widened_stop_at_reports_the_sub_candle_lead_instant_on_h1`
      (asserts 20:30Z).
- [x] core 862 / engine 156 pass; clippy clean; fmt run.

## DONE — exit sim now honours the widen (replay==live, shared code)
`simulate_fill_windowed` now reconstructs the System-2 widen via the SHARED
`widened_stop_at` and applies the transient widened stop in `[effective_from,
restored_at)` when scoring the exit — so a spread-hour spike that clears the
widened stop no longer books a false stop-out. The widen only PROTECTS (moves the
stop away); an overrun spike still stops out, but at the WIDENED level (the stop
the live broker actually held), not the original.
- [x] `SpreadWiden` gains `effective_from` (widen bar open) so the exit loop can
      gate per-bar; `at` stays the sub-candle display instant.
- [x] `widened_stop_at` reference price aligned to `original_stop` (matches the
      live cron's `blackout_apply::widen_one`, which uses `original_sl`) → widened
      stop matches the live broker's to the pip.
- [x] report.rs:690 comment corrected (no longer "un-widened bracket").
- [x] Tests: `simulate_fill_survives_spread_hour_spike_via_the_widened_stop`,
      `simulate_fill_stops_out_at_the_widened_stop_when_the_spike_overruns_it`.
      engine 158 / core 862 pass; clippy clean; fmt run.
- [x] VERIFIED end-to-end on the real plan `/tmp/tv-arm-replay-hs-gbp-aud-
      89697831.json`: entry #1 widens at 06:30, survives the 07:00 spike, TOOK
      PROFIT +4.96R (was −1.00R stop-out). Plan Net R −2.00 → +4.96.

### BE-before-widen edge (noted, not blocking)
`widened_stop_at` widens from the resolved/floored original SL, not a
break-even-moved stop. If BE armed BEFORE the widen, live remembers the
BE stop as the "original" and widens from there; the replay widens from the
signed original. Rare (BE arms at 50%-to-TP; a spread hour usually hits first)
and both still protect. Refine only if a real trade shows the drift.

## Note
The mask over-flag half (OANDA GBP_AUD [20,21]) is a SEPARATE change (regenerate
the mask) — out of scope here per the bug report.
