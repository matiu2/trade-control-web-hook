-- Phase 0 schema for PgStateStore — the native (VM + Postgres) replacement for
-- the Cloudflare KV-backed StateStore.
--
-- Design notes:
--  * One typed table per state family. Replaces the KV string-key encoding
--    (`veto:<scope>:<trade_id>:<instr>:<name>`) and the JSON index-blob hack
--    (`index:vetos` etc) — listing is now a plain SELECT, no RMW.
--  * TTL: control rows carry `expires_at timestamptz`; reads filter
--    `WHERE expires_at > now()`. Per-trade rows (plans, plan_state, archived,
--    entry_attempt, control_event) carry NO expiry (Bug #15 — a TTL there aged
--    out live state mid-trade). Those tables simply omit the column.
--  * Account scope: KV uses a `{scope}` segment where the global sentinel is a
--    fixed string and `Some(name)` is the account name. We store `account text`
--    NULL = global, non-null = that account. Global-first lookups become
--    `WHERE account IS NULL OR account = $1`.
--  * Structured bodies (TradePlan, PlanState, MwState, SpreadBlackoutRecord,
--    EntryAttempt, ControlEvent, NoEntryWindow[]) are stored as `jsonb` of the
--    exact serde shape the KV store serialised — same wire format, so parity is
--    a serialisation identity, not a re-modelling.

-- ─────────────────────────── control rows (TTL'd) ───────────────────────────

-- replay protection — was seen:<id>
CREATE TABLE IF NOT EXISTS seen (
  id          text PRIMARY KEY,
  action      text        NOT NULL,
  seen_at     timestamptz,
  outcome     text        NOT NULL DEFAULT '',
  trade_id    text,
  expires_at  timestamptz NOT NULL
);
CREATE INDEX IF NOT EXISTS seen_expiry ON seen (expires_at);

-- instrument cooldowns — was cooldown:<scope>:<instrument>
CREATE TABLE IF NOT EXISTS cooldown (
  account     text,                       -- NULL = global
  instrument  text        NOT NULL,
  set_at      timestamptz,
  expires_at  timestamptz NOT NULL,
  PRIMARY KEY (account, instrument)
);
CREATE INDEX IF NOT EXISTS cooldown_expiry ON cooldown (expires_at);

-- prep flags — was prep:<scope>:<instrument>:<step>
CREATE TABLE IF NOT EXISTS prep (
  account     text,                       -- NULL = global
  instrument  text        NOT NULL,
  step        text        NOT NULL,
  set_at      timestamptz NOT NULL,       -- the prep's `now`, used for ordering gate
  setter_id   text        NOT NULL DEFAULT '',
  expires_at  timestamptz NOT NULL,
  PRIMARY KEY (account, instrument, step)
);
CREATE INDEX IF NOT EXISTS prep_expiry ON prep (expires_at);

-- vetos — was veto:<scope>:<trade_id>:<instrument>:<name>
CREATE TABLE IF NOT EXISTS veto (
  account     text,                       -- NULL = global
  trade_id    text        NOT NULL,
  instrument  text        NOT NULL,
  name        text        NOT NULL,
  expires_at  timestamptz NOT NULL,
  PRIMARY KEY (account, trade_id, instrument, name)
);
CREATE INDEX IF NOT EXISTS veto_expiry ON veto (expires_at);

-- prep blocks — was prep-blocked:<scope>:<instrument>:<step>
CREATE TABLE IF NOT EXISTS prep_block (
  account     text,                       -- NULL = global
  instrument  text        NOT NULL,
  step        text        NOT NULL,
  set_at      timestamptz NOT NULL,
  expires_at  timestamptz NOT NULL,
  PRIMARY KEY (account, instrument, step)
);
CREATE INDEX IF NOT EXISTS prep_block_expiry ON prep_block (expires_at);

-- blackout pauses — was pause:<trade_id>:<blackout_id> (no account scope)
CREATE TABLE IF NOT EXISTS pause (
  trade_id    text        NOT NULL,
  blackout_id text        NOT NULL,
  reason      text,
  set_at      timestamptz NOT NULL,
  expires_at  timestamptz NOT NULL,
  PRIMARY KEY (trade_id, blackout_id)
);
CREATE INDEX IF NOT EXISTS pause_expiry ON pause (expires_at);

-- news windows — was news:<trade_id>:<news_id> (no account scope)
CREATE TABLE IF NOT EXISTS news_window (
  trade_id    text        NOT NULL,
  news_id     text        NOT NULL,
  reason      text,
  set_at      timestamptz NOT NULL,
  expires_at  timestamptz NOT NULL,
  PRIMARY KEY (trade_id, news_id)
);
CREATE INDEX IF NOT EXISTS news_window_expiry ON news_window (expires_at);

-- multi-shot dedup of same-signal-bar fires — was retry-fire:<scope>:<trade_id>:<shell_time>
CREATE TABLE IF NOT EXISTS retry_fire (
  account     text,
  trade_id    text        NOT NULL,
  shell_time  timestamptz NOT NULL,
  expires_at  timestamptz NOT NULL,
  PRIMARY KEY (account, trade_id, shell_time)
);
CREATE INDEX IF NOT EXISTS retry_fire_expiry ON retry_fire (expires_at);

-- global spread-blackout window marker (singleton) — was a single KV key
CREATE TABLE IF NOT EXISTS spread_blackout_window (
  singleton   boolean     PRIMARY KEY DEFAULT true CHECK (singleton),
  body        jsonb       NOT NULL,       -- SpreadBlackoutWindow
  expires_at  timestamptz NOT NULL
);

-- per-instrument market-hours entry blackout windows — was blackout-windows:<instrument>
CREATE TABLE IF NOT EXISTS blackout_windows (
  instrument  text        PRIMARY KEY,
  windows     jsonb       NOT NULL,       -- Vec<NoEntryWindow>
  updated_at  timestamptz NOT NULL,
  expires_at  timestamptz NOT NULL
);
CREATE INDEX IF NOT EXISTS blackout_windows_expiry ON blackout_windows (expires_at);

-- per-trade spread-blackout recovery records — was spread-blackout-record:<trade_id>
CREATE TABLE IF NOT EXISTS spread_blackout_record (
  trade_id    text        PRIMARY KEY,
  body        jsonb       NOT NULL,       -- SpreadBlackoutRecord
  expires_at  timestamptz NOT NULL
);
CREATE INDEX IF NOT EXISTS spread_blackout_record_expiry ON spread_blackout_record (expires_at);

-- evolving M/W geometry — was mw-state:<scope>:<trade_id>
CREATE TABLE IF NOT EXISTS mw_state (
  account     text,                       -- NULL = global (global-first lookup)
  trade_id    text        NOT NULL,
  body        jsonb       NOT NULL,       -- MwState
  expires_at  timestamptz NOT NULL,
  PRIMARY KEY (account, trade_id)
);
CREATE INDEX IF NOT EXISTS mw_state_expiry ON mw_state (expires_at);

-- ───────────────────────── per-trade rows (NO TTL) ──────────────────────────

-- registered plans — was plan:<scope>:<trade_id>, written with no expiry
CREATE TABLE IF NOT EXISTS trade_plan (
  account     text,                       -- NULL = global
  trade_id    text        NOT NULL,
  body        jsonb       NOT NULL,       -- TradePlan
  PRIMARY KEY (account, trade_id)
);

-- engine FSM state — was plan-state:<scope>:<trade_id>, no expiry (Bug #15)
CREATE TABLE IF NOT EXISTS plan_state (
  account     text,
  trade_id    text        NOT NULL,
  body        jsonb       NOT NULL,       -- PlanState
  PRIMARY KEY (account, trade_id)
);

-- placed entry attempts (multi-shot retry gate) — was entry-attempt:<scope>:<trade_id>:<attempt_no>
CREATE TABLE IF NOT EXISTS entry_attempt (
  account         text,
  trade_id        text        NOT NULL,
  attempt_no      integer     NOT NULL,
  body            jsonb       NOT NULL,   -- EntryAttempt (incl. broker_trade_id, set lazily)
  PRIMARY KEY (account, trade_id, attempt_no)
);

-- durable control-event audit trail — was control-event:<scope>:<trade_id>:<suffix>, no TTL
CREATE TABLE IF NOT EXISTS control_event (
  account     text,
  trade_id    text        NOT NULL,
  key_suffix  text        NOT NULL,       -- event.key_suffix(); append-only per (trade,suffix)
  seq         bigserial   NOT NULL,       -- append order within a trade
  body        jsonb       NOT NULL,       -- ControlEvent
  set_at      timestamptz NOT NULL,       -- ordering for list_control_events (ascending)
  PRIMARY KEY (account, trade_id, key_suffix, seq)
);
CREATE INDEX IF NOT EXISTS control_event_order ON control_event (account, trade_id, set_at);

-- archived (terminated) plans — was archived-plan:<scope>:<trade_id>, no TTL
CREATE TABLE IF NOT EXISTS archived_plan (
  account     text,
  trade_id    text        NOT NULL,
  body        jsonb       NOT NULL,       -- ArchivedPlan (plan + final_state + archived_at)
  PRIMARY KEY (account, trade_id)
);
