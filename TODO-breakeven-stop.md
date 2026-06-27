# TODO: break-even stop at 50%-to-TP (BUG-replay-no-breakeven-stop-at-50pct)

Once a candle **closes** past 50% of the entry→TP distance (in the trade
direction), move the stop-loss to **break-even = exact entry price**. One-way,
latched (arms once, never reverts). Both consumers honour it:

- replay-candles (engine `simulate_fill`)
- live worker (cron, via `amend_stop` on the open position)

Shared, signed data lives on the enter `Intent` (`breakeven` field) + a pure
helper in `core` so the two consumers can't drift (same pattern as
`entry_level_vetos` Bug #12 / `pause_gate`).

## Design (confirmed with operator 2026-06-28)

- Encoding: **field on enter Intent** (not a separate ConditionRule).
- BE target SL: **exact entry price** (0R scratch).
- Trigger basis: **candle close past the entry→TP midpoint**, not a wick.
- Threshold: **50%** of entry→TP (tunable; start at 0.5).
- Latched: arms once; the SL is moved and stays. Broker's SL catches the rest.
- Same-bar: BE arms on a close; the moved SL is live from the **next** bar.
  The original SL / close-on-reversal still apply on the arming bar (the
  broker SL handles intrabar). "Closing past 50% only needs handling once."

## Steps

- [x] 1. core: `Breakeven` struct + `arms_at` / `close_arms` / `target_stop`
       pure helpers (`core/src/intent/breakeven.rs`), 8 tests. Re-exported.
- [x] 2. core: `Intent.breakeven: Option<Breakeven>` (serde default-absent,
       signed). `Resolved.breakeven` threaded through `from_intent`. All 16+3
       struct literals updated.
- [x] 3. engine `simulate_fill`: latched BE arm in the exit walk — a candle
       that CLOSES past the 50% level moves `active_stop` to entry for
       subsequent bars. 2 tests (trade-075 leg-2: −1R→0R; wick does not arm).
       Core+engine suites green (677). clippy+fmt clean.
- [ ] 4. live worker: cron step that, for open positions of a trade whose enter
       carried `breakeven`, arms once a closed candle passes 50% and calls
       `amend_stop(entry)`. (Re-use the blackout_watch open-position pattern.)
- [ ] 5. tv-arm / build-trade: bake `breakeven` onto the `05-enter` intent
       (default on at 50%? confirm gate). README.
- [ ] 6. README + CHANGELOG + tag; advance parent submodule pointer.

A change isn't done until: tests pass, clippy clean, fmt run.
