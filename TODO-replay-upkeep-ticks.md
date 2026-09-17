# TODO — replay walks the 15-minute upkeep ticks (job 2)

Branch: `feat/replay-upkeep-ticks`
Worktree: `../trade-control-web-hook-upkeep-ticks` (sibling — path-dep rule)
Source: memory `job_replay_walks_the_15_minute_upkeep_ticks`.

## Why

Live runs `order_control_tick::run_both` (promote, then re-price) every
`upkeep_secs` = 900 s off a live quote. Replay runs the same two shared passes
ONCE per plan bar off that bar's CLOSE spread. On D1 the close is the 17:00-NY
rollover print, so a widened stop is never shrunk back and a Stored order is
never promoted offline — every D1 fixture's R is unreliable.

**Constraint (operator): shared code.** Same `promote_due_orders` /
`reprice_due_orders` / `sl_target` / `run_enter` sizing. Only the clock and
the quote source differ. No replay-local sizing or widen decision.

## Plan (first cut; default byte-identical)

- [x] 1. `ReplayBroker::set_upkeep_sample(Option<BidAskCandle>)` — when set,
      `get_quote` reads the sample's `bid_c/ask_c` and keys the spread-hour
      clamp on the sample's time. Held state (`as_of`) untouched. Tests.
- [x] 2. `cli/src/bin/replay_candles/upkeep.rs`: `UpkeepTicks` — a finer
      bid/ask series (M15 / H1) over the live window; `samples_in(open, close)`
      yields sub-bars whose CLOSE falls strictly inside `(open, close)`.
      Tests (boundaries excluded, ascending, empty).
- [x] 3. `replay::run_with_upkeep(.., Option<&UpkeepTicks>)`; `run` delegates
      with `None`. Per tick, BEFORE `evaluate_plan` (after `set_as_of(bar_open)`
      / `advance`), walk the samples in `(batch_open, now)`: set the sample,
      call `promote_due_orders` + `reprice_due_orders` with `now` = sample
      close; clear the sample after. Mutation test at THIS entry point: an
      upkeep series with a narrow mid-bar spread must shrink a widened resting
      stop that the per-bar (wide-close) pass alone leaves floored.
- [x] 4. Driver: `--upkeep <GRAN>` (off by default). Pull the finer bid/ask
      series over the live window via `candles::pull`; pass to both zoom
      passes. `--rebless` refuses under `--upkeep` (same shape as `--cron-gap`).
      Fixture replay stays `None` (offline, byte-identical).
- [x] 5. clippy + fmt green. Corpus: `all_fixtures_match_expected` and the
      uk-100 expiry test fail on this branch AND on a clean `staging` baseline
      worktree (same uk-100 fixture; the primary checkout already carries an
      uncommitted edit to its expected.json) — pre-existing drift, not this
      change. No D1 fixture exists in the corpus; smoke on the H4 EUR/CAD
      fixture with `--upkeep 1h` / `15m` pulls 304 / 1216 bars and walks them
      (report unchanged: no resting order there). Committed 9ff8ccaf, pushed feat/replay-upkeep-ticks.

## Out of scope / follow-ups (from the job)

- `windowed_entry_spread` sampling H1 regardless of plan granularity —
  "decide with the corpus, not by argument".
- Forecast-term multiple (~2–3× worst hour) — measure as a replay axis first.
- Fixture-frozen upkeep series (`upkeep_bars.json`) so fixtures can carry it.
- Intra-bar fill timing: the fill sim is still per plan bar, so an order the
  bar fills is not re-priced by that bar's sub-bar ticks (it is already filled
  as of `bar_open`). Accepted first-cut limitation.
