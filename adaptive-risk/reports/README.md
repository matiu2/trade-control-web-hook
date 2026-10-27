# Provisional simulation and validation

`provisional-10000.json` embeds the policy, model inputs, confidence level and 10,000 paths for each of four configurations, seed 42, 200 opportunities per path, initial balance 10,000, target 10%, default static/balance risk policy. Outcomes use provisional 58% wins at +2.13R and 42% losses at -1R. Trades/day is modeled as uniform over {1,2,3}; each trade completes before the next. These are synthetic results, not journal-derived or live success probabilities. Re-run the example after changing inputs; preserve dataset provenance.

All four modes use the same reserve-aware volume sizing. Net R multiplies nominal approved stop exposure, not the larger unspent risk reservation. `average_risk_pct` is nominal risk; `average_reserved_risk_pct` includes the reserve.

`net_return_pct`, `max_drawdown_pct`, `longest_losing_streak`, `skipped_trades`, `average_risk_pct`, `daily_lockouts`, `account_limit_breaches`, `daily_firm_limit_breaches`, and conditional `trades_to_target` carry normal 95% confidence intervals on their means. `funding_probability` is a fraction with a Wilson 95% interval, where funding means reaching the modeled target before any modeled firm breach. It omits other real-world evaluation requirements.

The actual pinned broker workspace fails before feature compilation because the pinned strategy-common commit declares `windows` without committing its source file. Other workspace members reference sibling projects not initialized/present in this worktree. Validation therefore also used a broker-only copy of the workspace, providing the existing local `windows.rs` only in that copy. Broker baseline: 106 passed, 2 ignored. Final isolated validation: broker 115 passed, 2 ignored; core 1,380 passed. The independent risk crate's policy, persistence and replay tests and its Clippy/format gates passed. Core/broker Clippy with warnings denied and formatting checks passed. This feature does not repair or advance those dependency pins. The risk crate has its own workspace/lockfile and can be tested directly.

Protocol fields were checked against Spotware's official [messages](https://github.com/spotware/openapi-proto-messages/blob/main/OpenApiMessages.proto) and [models](https://help.ctrader.com/open-api/model-messages/). The latter explicitly excludes potential closing commission from net unrealized P&L; it must be reserved separately. No broker-authenticated live mutation was executed during implementation, and the purchased program's actual rules and fee conventions remain unverified.

## Model results

Each cell below is mean [95% confidence interval]. Target success is the model proxy described above.

| Configuration | Net return % | Maximum drawdown % | Target success % | Nominal risk % | Reserved risk % |
|---|---:|---:|---:|---:|---:|
| Fixed | 10.71 [10.69, 10.73] | 2.12 [2.10, 2.15] | 99.74 [99.62, 99.82] | 0.83 [0.83, 0.83] | 1.00 [1.00, 1.00] |
| StreakOnly | 10.80 [10.79, 10.81] | 1.84 [1.82, 1.86] | 99.95 [99.88, 99.98] | 0.72 [0.71, 0.72] | 0.86 [0.86, 0.86] |
| DrawdownOnly | 10.76 [10.75, 10.77] | 2.10 [2.08, 2.13] | 100.00 [99.96, 100.00] | 0.83 [0.83, 0.83] | 0.99 [0.99, 0.99] |
| Combined | 10.81 [10.80, 10.82] | 1.84 [1.82, 1.86] | 100.00 [99.96, 100.00] | 0.71 [0.71, 0.72] | 0.86 [0.86, 0.86] |

| Configuration | Losing streak | Skipped trades | Daily lockouts | Account breaches | Trades to target (conditional) |
|---|---:|---:|---:|---:|---:|
| Fixed | 2.59 [2.56, 2.62] | 0.00 [0.00, 0.00] | 0.00 [0.00, 0.00] | 0.00 [0.00, 0.00] | 15.74 [15.59, 15.88] |
| StreakOnly | 2.75 [2.72, 2.78] | 0.00 [0.00, 0.00] | 0.00 [0.00, 0.00] | 0.00 [0.00, 0.00] | 19.16 [18.96, 19.35] |
| DrawdownOnly | 2.60 [2.57, 2.63] | 0.00 [0.00, 0.00] | 0.00 [0.00, 0.00] | 0.00 [0.00, 0.00] | 16.02 [15.86, 16.18] |
| Combined | 2.75 [2.72, 2.78] | 0.00 [0.00, 0.00] | 0.00 [0.00, 0.00] | 0.00 [0.00, 0.00] | 19.24 [19.04, 19.43] |
