# BUG — daily/H4 plans on non-FX CFDs fire inside the market-hours close block

**Status:** PARTLY WRONG + PARTLY OPEN (2026-09-19). Measured: the gate blocks only the close HOURS (Spain 35: 15,16Z), so a 21Z daily enter was never rejected by it — it reached a closed market. The grid is now per-instrument, but a bar ending at the session close still "closes" when the market is shut; acting on it at the next open is an open operator decision.

*(original report below)*


**Status:** recorded 2026-09-18, deferred. Most work is FX; ignore until a
trade needs it. Companion: `BUG-d1-h4-grid-should-follow-instrument-session.md`.

## What happens

Every D1 and H4 bucket sits on ONE grid, 17:00 America/New_York
(`candle-cache::session_anchor`, mirrored in `tradenation-api`). Neither takes an
instrument. So a daily bar for a European index or share CFD "closes" at 21/22Z,
hours after its exchange shut.

The engine fires on that bar close. For the 13 instruments in
`core/src/market_hours_baked.rs` that carry a weekday `daily_close_hours`
overlay (Euro Stocks 50, Switzerland 20, Spain 35, Germany 40 / UK 100 diff,
South Africa 40, ASTRAZENECA, the TRY crosses, UK10YB…), the NY-anchored close
lands inside — or right after — their blocked close hour, so
`intent::market_hours_blocked` **rejects every daily enter** on them.
Operator decision 2026-09-18: the market-hours gate rejects on ALL
granularities (no delay), so this is consistent behaviour, not a gate bug —
but it means daily plans on those CFDs cannot enter at all today.

## Why it's not visible

The corpus is 2309 H1 / 240 H4 / 298 M15 fixtures, effectively all FX. No daily
fixture exists on any instrument.

## Fix direction

Either (a) accept "no daily plans on session-bound CFDs", or (b) fix the bar
grid per instrument — see the companion bug. (b) is the real fix; the gate
itself is right.
