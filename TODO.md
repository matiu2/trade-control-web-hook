# TODO — per-instrument D1/H4 session anchor (webhook side)

- [x] bump `tradenation-api` to `broker-tradenation-v0.20.0`; `[patch]` instrument-lookup to the local path
- [x] TN adapter: both fetch paths use `get_candles_range_aggregated_on` + `session_anchor(instrument)`
- [x] CLAUDE.md hazards; BUG doc statuses
- [x] repair the stale-binary re-bless (separate commit)
- [x] cache migration: `rebuild-h4` on the whole TN table + anchored OANDA names; TN Australia 200 D rows deleted
- [x] `local-chart`: no code change needed (bars come through candle-cache); restart it
- [ ] replay: `replay.rs:567` NY-close edge sampling assumes H4 lands on the NY close — only matters for the spread-blackout marker on anchored instruments (their masks are not NY-based anyway); not changed
- [x] session spans baked (`market-hours-gen --session-out`), `market_session`, `StoredReason::MarketClosed` park + promote at the open
- [x] replay: newest park wins in `armed_verified`; 32 goldens re-blessed with a note
