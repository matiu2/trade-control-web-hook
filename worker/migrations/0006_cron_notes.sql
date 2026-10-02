-- A third event kind for `plan timeline`, alongside `request_records`
-- (real inbound signed HTTP alerts) and `tick_bundles` (real cron-engine
-- `evaluate_plan` evaluations). Neither of those means "a cron pass made a
-- broker-truth observation that isn't a dispatched rule firing" — the shape
-- `trade-control-cron::reconcile` needed (see its module docs) — so rather
-- than stretch either existing meaning, this is its own table.
--
-- Same append-only/no-TTL shape as `0003_recordings.sql`'s pair: audit trail,
-- only `plan purge` deletes.

CREATE TABLE IF NOT EXISTS cron_notes (
  id              bigserial   PRIMARY KEY,             -- surrogate; insertion order
  ts              timestamptz NOT NULL,                -- CronNote.ts (observed instant)
  correlation_id  text        NOT NULL,                -- the plan's trade_id (aggregate key)
  account         text,                                -- None = global plan; Some = account-scoped
  source          text        NOT NULL,                -- which cron pass wrote this (e.g. "reconcile")
  severity        text        NOT NULL,                -- "info" | "warn" | "error" — mirrors tracing levels
  message         text        NOT NULL,                -- short one-line summary for the timeline
  body            jsonb       NOT NULL                 -- the whole CronNote, verbatim serde
);

-- Date-range scans and per-trade life replay, mirroring tick_bundles' pair.
CREATE INDEX IF NOT EXISTS cron_notes_ts_idx             ON cron_notes (ts);
CREATE INDEX IF NOT EXISTS cron_notes_correlation_id_idx ON cron_notes (correlation_id);
