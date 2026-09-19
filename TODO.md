# TODO — per-instrument D1/H4 session anchor (webhook side)

- [x] bump `tradenation-api` to `broker-tradenation-v0.20.0`; `[patch]` instrument-lookup to the local path
- [x] TN adapter: both fetch paths use `get_candles_range_aggregated_on` + `session_anchor(instrument)`
- [x] CLAUDE.md hazards; BUG doc statuses
- [x] repair the stale-binary re-bless (separate commit)
- [ ] cache migration: `rebuild-h4` per anchored instrument, delete their D rows (both Postgres tables)
- [ ] `local-chart` granularity.rs (separate repo)
- [ ] replay: `replay.rs:567` NY-close comment/logic assumes H4 lands on the NY close — check for anchored instruments
- [ ] OPEN (operator): act on a session's last bar at the next open
