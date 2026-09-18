# Re-bless 2026-09-18 (second) — masked-hour floor/forecast, and a stale-binary re-bless repaired

Base: staging 2241c56b (`feat/stop-floor-masked-hours`, the other session's
floor + forecast change). 304 goldens moved, for two unrelated reasons.

## 1. 274 goldens: the operator's re-bless 97ac98f1 used a stale binary

97ac98f1 re-blessed ~210 goldens and added 64 fixtures with a `replay-candles`
older than a0dcd6da, so every promoted park's leg (see
`REBLESS-2026-09-18-promoted-parks-and-spread-gate.md` §1) dropped back out
of those goldens. Measured: the corpus gate at 97ac98f1 was RED on 282
fixtures. Example `aud-cad-h1-2026-07-22-normal-news-off`: 1 leg +0.26R →
2 legs +0.56R. This re-bless restores them. Nothing about the strategy moved.

Lesson: re-bless from a fresh `cargo run`, not the installed binary; the gate
test (`all_fixtures_match_expected`) is the check.

## 2. 30 goldens: spread-hour samples excluded from the floor and the forecast

`closes_outside_spread_hours` drops a window bar whose close instant is a
spread hour before the trailing mean; `spread_forecast_frac` returns 0 for a
masked hour. Stop moves are sub-pip (e.g. 1.60361 → 1.60355); 29 H4 + 1 M15.
No H1 golden moved on this alone.

Not re-blessed, still pre-existing: `uk-100-news-blackout-rentry-close-on-reversal`.
