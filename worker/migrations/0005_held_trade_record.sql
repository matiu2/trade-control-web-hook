-- Rename `spread_blackout_record` → `held_trade_record`.
--
-- The table long predates what it now holds. It started as a spread-hour
-- recovery record; it now carries the resting-order HOLD refcount
-- (`Holders`/`HoldReason`, v120) whose reasons include a news pause, which has
-- nothing to do with spreads. Both of the things it remembers for restoration —
-- widened open-position stops (System 2, `original_stops`) and cancelled pending
-- orders (System 3, `cancelled_orders`) — are "what this trade is holding", so
-- the name follows the refcount's vocabulary.
--
-- NOTE this is NOT the global `spread_blackout_window` singleton, nor the
-- per-instrument `blackout_windows` table. Those remain genuinely
-- spread/market-hours concepts and keep their names.
--
-- `ALTER TABLE ... RENAME` preserves the rows, so in-flight holds survive the
-- deploy — important, since dropping them would strand every currently-cancelled
-- resting order until its 12h backstop. `IF EXISTS` keeps this idempotent and
-- lets a database created after the rename (which never had the old table) apply
-- the file cleanly.
ALTER TABLE IF EXISTS spread_blackout_record RENAME TO held_trade_record;

-- The index and the primary-key constraint follow the table (Postgres keeps them
-- attached across the rename, but under their old names). Rename both so the
-- schema reads consistently — a stale `spread_blackout_record_pkey` on a table
-- called `held_trade_record` is exactly the confusion this migration removes.
ALTER INDEX IF EXISTS spread_blackout_record_expiry RENAME TO held_trade_record_expiry;
ALTER INDEX IF EXISTS spread_blackout_record_pkey RENAME TO held_trade_record_pkey;

-- Fresh databases: `0001_state.sql` still creates the table under its old name
-- (its checksum is frozen — sqlx verifies applied migrations, so it must not be
-- edited), and the rename above then applies. This guard covers the case where
-- neither ran, so the table exists either way.
CREATE TABLE IF NOT EXISTS held_trade_record (
  trade_id    text        PRIMARY KEY,
  body        jsonb       NOT NULL,       -- HeldTradeRecord
  expires_at  timestamptz NOT NULL
);
CREATE INDEX IF NOT EXISTS held_trade_record_expiry ON held_trade_record (expires_at);
