-- Recording sink — the native replacement for the two Cloudflare R2 prefixes:
--   * `req/`   — webhook `RequestRecord`s (`src/recording.rs`).
--   * `ticks/` — cron-engine `TickBundle`s (`core/src/tick_bundle.rs`,
--                written by `src/tick_recording.rs`).
--
-- Both R2 layouts are write-once JSON objects keyed
-- `<prefix>/<date>/<ts>-<id>.json`, scanned downstream by date prefix and
-- filtered by `trade_id`. Postgres stores the whole record as `jsonb` (the
-- exact serde shape the R2 object held, so a downstream replay deserialises it
-- byte-for-byte) plus the extracted correlation columns the R2 key encoded, so
-- the same date-range + trade-keyed queries are plain indexed SELECTs instead
-- of an object-store prefix glob.
--
-- Append-only, no TTL (these are the audit/replay trail — the
-- `plan purge` / `purge --older-than` ops are the only deleters, mirroring the
-- per-trade no-TTL rule for plan-state rows). `body` is `jsonb NOT NULL`; the
-- correlation columns are nullable where the source field is `Option`.

-- Webhook request recordings (the `req/` prefix).
CREATE TABLE IF NOT EXISTS request_records (
  id          bigserial   PRIMARY KEY,                 -- surrogate; insertion order
  ts          timestamptz NOT NULL,                    -- RequestRecord.ts (received instant)
  request_id  text        NOT NULL,                    -- minted correlation id (FNV of body+headers)
  intent_id   text,                                    -- RequestRecord.intent_id (if it parsed)
  trade_id    text,                                    -- RequestRecord.trade_id (the aggregate key)
  status      int         NOT NULL,                    -- final HTTP status returned
  outcome     text        NOT NULL,                    -- short outcome string
  body        jsonb       NOT NULL                     -- the whole RequestRecord, verbatim serde
);

-- Date-range scans (was the `req/<date>/…` prefix) and trade-keyed
-- reconstruction (was the `trade_id` field filter).
CREATE INDEX IF NOT EXISTS request_records_ts_idx        ON request_records (ts);
CREATE INDEX IF NOT EXISTS request_records_trade_id_idx  ON request_records (trade_id) WHERE trade_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS request_records_intent_id_idx ON request_records (intent_id) WHERE intent_id IS NOT NULL;

-- Cron-engine tick recordings (the `ticks/` prefix). One row per recorded
-- `(tick, plan)` evaluation; no-op ticks are trimmed at the write side
-- (PlanEval::is_noteworthy), exactly as the R2 writer trims them.
CREATE TABLE IF NOT EXISTS tick_bundles (
  id              bigserial   PRIMARY KEY,             -- surrogate; insertion order
  tick_ts         timestamptz NOT NULL,                -- TickBundle.tick_ts (cron instant)
  correlation_id  text        NOT NULL,                -- the plan's trade_id (aggregate key)
  account         text,                                -- None = global plan; Some = account-scoped
  request_id      text        NOT NULL,                -- per-(tick,plan) causal-chain id
  schema_version  int         NOT NULL,                -- TickBundle.schema_version
  body            jsonb       NOT NULL                 -- the whole TickBundle, verbatim serde
);

-- Date-range scans (was `ticks/<date>/…`) and per-trade life replay (was the
-- `<ts>-<trade_id>` key tail).
CREATE INDEX IF NOT EXISTS tick_bundles_tick_ts_idx        ON tick_bundles (tick_ts);
CREATE INDEX IF NOT EXISTS tick_bundles_correlation_id_idx ON tick_bundles (correlation_id);
