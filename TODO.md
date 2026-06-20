# TODO — replay-candles: pull window from TradingView when flags absent — DONE (v47)

Goal: `replay-candles` should derive instrument + granularity + start + end
straight from the **current TradingView chart state** (replay-mode visible
window) when those CLI flags are omitted. Flags remain optional overrides.

Workflow this enables:
1. TV replay mode → rewind back in time.
2. `tv-arm --plan-out plan.json` builds the plan.
3. Scrub TV forward to the end of the trade.
4. `replay-candles --plan plan.json` reads the chart's current visible window
   (instrument + granularity + start/end) and replays exactly that span.

## Building blocks (verified in `trading-view` crate)
- `TvMcp::get_state() -> ChartState { symbol: "OANDA:EURUSD", resolution: "60" }`
  → instrument + granularity in one call.
- `TvMcp::get_range() -> ChartRange { visible_range: UnixRange { from, to } }`,
  `UnixRange::to_utc() -> (DateTime<Utc>, DateTime<Utc>)` → window start/end.
- TV resolution codes → granularity: `"1"|"5"|"15"|"60"|"240"|"D"` (mirrors
  tv-arm's `resolution_to_granularity`).
- `--tv-mcp-root` flag mirrors tv-arm.

## Tasks
- [x] add `trading-view` path-dep to `cli/`
- [x] new module `replay_candles/tv.rs`:
      - `resolution_to_friendly(&str) -> Option<&'static str>` (TV code → "1h" etc.)
      - strip `EXCHANGE:` prefix off `ChartState.symbol` → bare TV symbol
      - `pull_defaults(&TvMcp) -> TvDefaults { instrument, granularity, start, end }`
      - unit tests: resolution map; strip exchange prefix (live MCP not tested)
- [x] `replay_candles.rs`:
      - make `--start` optional; add `--tv-mcp-root` flag
      - when instrument/granularity/start/end absent → fetch from TV (lazy: only
        call MCP if at least one is missing)
      - keep flags as overrides; granularity-vs-plan mismatch check still applies
- [x] gate: cargo test / clippy -D warnings / fmt in trade-control-web-hook
- [x] wasm ring-free: `cargo tree -p trade-control-web-hook | grep -i ring` empty,
      no candle-cache/trading-view in worker cdylib tree
- [x] CHANGELOG.md v47 entry; merge to main; tag v47
