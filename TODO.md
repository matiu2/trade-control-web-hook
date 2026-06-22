# TODO — replay-candles smarter defaults from TV replay cursor + plan

Branch: `feat/replay-cursor-from-plan` (worktree). Target env: **dev** (`main`).

Operator workflow: start TV in **replay mode at the start of the trade**, then
just run `replay-candles-dev --plan plan.json`. The chart's last shown candle is
the replay cursor (= trade start); the plan carries the trade-expiry (= end) and
the granularity. So none of `--start`/`--end`/`--granularity` need to be typed.

## Changes

- [x] **start = last shown candle** (`bars_range.to`, the replay cursor), not
      `visible_range.from`.
- [x] **end = plan's trade-expiry** (`Trigger::TimeReached.at_epoch` on the rule
      whose `rule_id` contains `trade-expiry`); fall back to chart
      `visible_range.to` when the plan has no such rule.
- [x] **granularity from the plan** (`plan.granularity`), not the chart. CLI
      `--granularity` flag still overrides. Dropped chart-resolution reading from
      the `need_tv`/`resolve_window` path.
- [x] instrument unchanged: `--instrument` flag → chart symbol → plan.

## Touch points

- `cli/src/bin/replay_candles/tv.rs` — `TvDefaults`/`pull_defaults`: drop
  granularity (move to plan), switch start to `bars_range.to`, end to
  `visible_range.to` (fallback only).
- `cli/src/bin/replay_candles.rs` — `resolve_window`: granularity from plan;
  end from plan trade-expiry with chart fallback; recompute `need_tv`.
- helper to extract trade-expiry epoch from a `TradePlan`.

## Done when

- [x] new unit tests for trade-expiry extraction + granularity resolution
- [x] `cargo test -p trade-control-cli` green (13 + 20 bin tests)
- [x] `cargo clippy` clean, `cargo fmt` run
- [x] CHANGELOG v54 added (replay-candles not in README; documented in CHANGELOG)
- [ ] commit + push + advance parent gitlink
- [ ] rebuild + reinstall via deploy scripts so `replay-candles-<env>` updates
