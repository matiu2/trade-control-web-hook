# TODO — entry floor: drop spread-hour closes from the windowed spread sample

Branch: `feat/entry-floor-skips-spread-hour`  (worktree `../trade-control-web-hook-upkeep-ticks`)
Design item 2 of job 2; follows the H4+ spread-gate delay.

## Why
`run_enter`'s SL floor = 10× the mean `ask_c − bid_c` of the last 5 closed
bars at the plan granularity (`windowed_entry_spread` → `trailing_spread_mean`).
A bar whose CLOSE falls in the instrument's baked spread hour (the 17:00-NY
rollover print) leaks that spike into the mean: on H1 one bar in five, on H4
one in six, on D1 every bar. The operator's rule: no entry in the spread hour,
so the hour's prints must not size the stop either.

## Steps
- [x] 1. `broker::trailing_spread_mean_outside_spread_hour(instrument, bar_secs, candles, window)`
      (shared, pure): drop bars whose close is `is_spread_hour`, then the
      existing reducer. `windowed_entry_spread` calls it. Padding: `lookback_bars`
      already over-fetches (slack + gap bars) so `window` clean bars survive.
- [x] 2. Entry-point test in `run_enter`: newest H1 bar closes 21:00Z with a
      30-pip book, four tight bars before → stop NOT widened (mutation: include
      the bar → widened). D1 mirror: every bar in the hour → falls back to the
      live quote.
- [ ] 3. Corpus delta measured; re-bless with a note; clippy + fmt; commit + push.
