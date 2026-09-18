# TODO — SL-spread floor: exclude masked-hour samples (branch feat/stop-floor-masked-hours)

Stacked on `feat/spread-gate-park` (3eced28f). Step 4 of the 2026-09-18 spread-gate
plan; steps 1–3 (park on H4+, promote release, replay clamp removal) shipped there.
Worktree `../trade-control-web-hook-stop-floor-mask` (sibling — path-dep rule).

## Why

The entry floor sizes off the mean close spread of the last 5 plan bars. On D1
(17:00 NY grid) EVERY close is the rollover print, and on H1 the 17:00 bar is in
the window one bar in five, so the floor is sized off the very spike the
SpreadHour hold already keeps the order out of (AUD/NZD D1: 147p floor vs 55p
drawn). The forecast term does the same thing forward: at 16:xx NY it widens a
resting order for an hour the lifecycle is about to cancel it through anyway.

## Steps

- [x] 1. `spread_blackout::closes_outside_spread_hours(instrument, candles, bar_seconds)`
      — pure; keeps bars whose CLOSE instant is not `is_spread_hour`. Tests: keyed
      on close not open; empty in → empty out; unmasked instrument passes through.
- [x] 2. `spread_forecast_frac` returns 0.0 for a masked hour (this or next).
      Flip the two forecast tests that pinned the spike; keep the raw-column tests.
- [x] 3. `enter.rs::windowed_entry_spread` filters via (1) before the shared mean;
      an all-masked window → `None` → live-quote fallback (logged). Entry-point
      test: 5 D1 bars all closing at 17:00 NY with a 20p spread must NOT widen /
      reject a 10p-stop EUR/USD enter when the live quote is 1p. Mutation: drop the
      filter → rejected.
- [x] 4. README floor section + CLAUDE.md note; clippy + fmt; commit + push.
- [ ] 5. Corpus run + re-bless: OPERATOR does this at the end. Do not `--rebless` here.
