# TODO — journal trade recording (SQLite)

Record each journalled trade so we can later query win-rate / R / expectancy
sliced by entry-mode (quasimodo…), instrument class (index…), broker, direction.
Record the **replay outcome** and the **real-life outcome** SEPARATELY.

Decisions (confirmed with user 2026-07-23):
- **Trigger**: explicit `s` key on the Compare screen (both outcomes loaded there).
- **Depth**: stats only — numeric/enum columns, no raw JSON blobs.
- **Storage**: SQLite via `rusqlite` (pinned `0.36`, `bundled`, to dodge the
  workspace `libsqlite3-sys` `links` collision). One file per env at
  `~/.config/trade-control/journal-<env>.db`; upsert by `trade_id`.

## Schema (one row per trade)

Identity / dimensions:
- `trade_id` TEXT PRIMARY KEY
- `instrument` TEXT            -- raw plan id (AUD_CAD)
- `instrument_class` TEXT      -- from instrument-lookup asset.class (forex/index/gold/…)
- `broker` TEXT               -- oanda / tradenation
- `direction` TEXT            -- long / short
- `granularity` TEXT          -- h1 / m15 / …
- `entry_mode` TEXT           -- normal / quasimodo / strategy-v2 / unknown
- `order_type` TEXT           -- first enter leg's type (stop/limit/market)
- `armed_at` TEXT             -- RFC3339 UTC
- `recorded_at` TEXT          -- RFC3339 UTC, when this row was written

Real-life outcome:
- `live_entry_ts` TEXT NULL   -- Brisbane, from timeline (None = never entered)
- `live_outcome` TEXT         -- derive_outcome text (entered/rejected/closed…)
- `live_is_ok` INTEGER        -- derive_outcome bool

Replay outcome (parse_replay_outcome):
- `replay_done` INTEGER NULL
- `replay_final_phase` TEXT NULL
- `replay_fires` INTEGER NULL
- `replay_tp` INTEGER NULL
- `replay_sl` INTEGER NULL
- `replay_net_r` TEXT NULL     -- keep as text ("+0.50") to match report exactly

## Steps

- [x] Add `rusqlite` (0.36, bundled) — resolves the links collision.
- [ ] `record.rs`: `TradeRecord` struct + `open_db(path)` (migrate on boot) +
      `upsert(&conn, &rec)` + `instrument_class(instrument, broker)` helper
      (instrument-lookup) + `db_path()` (env-suffixed). Unit tests over an
      in-memory DB.
- [ ] `record.rs`: `TradeRecord::from_plan(detail, timeline_json, replay_report)`
      — assemble a record from the already-parsed sources. Unit test over fixtures.
- [ ] Wire into app: `App::record_current()` — build from current PlanData,
      upsert, set status. Guard: only on Compare with replay_report present.
- [ ] keys.rs: `s`/`S` on non-list screens → `Action::Record`; apply → record_current.
- [ ] ui: footer hint on Compare mentions `s record`; small confirmation in status.
- [ ] Tests: record_current upserts; missing-replay path errors cleanly.
- [ ] cargo test / clippy / fmt green.
- [ ] Update memory + CLAUDE-adjacent docs; merge to main; bump parent pointer.

## Not now (future)
- A query surface (a `journal stats` subcommand, or an in-TUI stats screen).
  For now we just RECORD; querying is done with `sqlite3` by hand.
