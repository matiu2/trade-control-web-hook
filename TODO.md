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
- [x] core 857 / engine 156 / cron 11 pass; clippy clean (pre-existing
      `for h in 0..24` warning only); fmt run.

## Note
The mask over-flag half (OANDA GBP_AUD [20,21]) is a SEPARATE change (regenerate
the mask) — out of scope here per the bug report.
