# BUG — D1/H4 bucket grid should follow the instrument's session for non-FX CFDs

**Status:** FIXED 2026-09-19 for every index row — per-instrument session anchor (see CLAUDE.md "D1/H4 bars start at the INSTRUMENT's session anchor"). Shares, bonds, Coffee and the LME metals are still on the FX day. `local-chart` not yet moved.

*(original report below)*


**Status:** recorded 2026-09-18, deferred. Do NOT fold into job 1
(`job_consistent_session_start_hour_across_toolchain`), which is FX-only and
correctly standardises one 17:00-NY grid. Ignore until a non-FX daily/H4 plan
is wanted.

## What happens

The D1 and H4 anchor is a single 17:00 America/New_York rule for every
instrument. For an exchange-session CFD such as Spain 35 (roughly 07:00–15:30
UTC cash session, broker extends to ~21Z) a NY-anchored daily bar is the wrong
container: its open is the previous evening's last print and it closes hours
after the market did. Depending on the broker's extended hours a "daily" bar
can hold two halves of two exchange days rather than one session.

H4 has the same shape: the 01/05/09/13/17/21 NY grid does not align with a
07:00 UTC European open, so the first H4 bar of the day straddles pre-open and
the opening auction.

The engine, the fixtures, the chart (`local-chart`) and the candle-cache keys
all agree with each other on the NY grid, so nothing *diverges* — they are all
consistently on a grid the exchange doesn't trade on.

## Fix direction

Per-instrument session anchor: D1 opens at the instrument's own market open,
H4 grid starts from it. The seam already exists — the spread-blackout baseline
table (`core/src/spread_baseline_candle.rs`) carries a per-instrument
`schedule` name (`ny`, `sydney`, …) with a timezone; nothing but the spread
hour reads it. A session anchor keyed on that schedule would need to land in:

1. `candle-cache::session_anchor` (bucket keys) — currently instrument-blind.
2. `tradenation-api` aggregator (H4 and, post job 1, D1 from H1).
3. OANDA request params (`dailyAlignment` / `alignmentTimezone`) per instrument.
4. `local-chart` bar rendering and `replay-candles` fixtures (re-bless + note;
   `[[rebless_can_silently_retire_coverage]]`).
5. The market-hours gate then stops rejecting daily enters on these
   instruments by construction (the bar closes at the session close, before
   the blocked hour). See the companion bug.

Check TradingView's grid for `TRADENATION:<index>` vs the exchange symbol
before choosing — a setup drawn on the exchange symbol will not line up
bar-for-bar with broker-grid bars.

Companion: `BUG-daily-plans-on-cfds-rejected-by-market-hours-gate.md`.
