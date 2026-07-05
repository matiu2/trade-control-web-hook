-- Order-body recovery cache — the native replacement for the KV `order:<id>`
-- slots (`src/state/kv.rs::order_body_key`).
--
-- `run_enter` persists the raw signed alert body for each placed order keyed by
-- the broker order id; the spread-blackout *apply* cron later finds a broker
-- *pending order* (not a signed intent) and needs the original signed bytes to
-- re-drive that entry on recovery via `incoming::parse_and_verify`.
-- `plan delete` / `plan purge` clean these up.
--
-- This was missed in the Phase-0 schema port (0001): at that point the three
-- order-body methods were inherent to `KvStateStore`, NOT on the `StateStore`
-- trait, so the conformance harness never exercised them. The dispatch
-- genericisation (`store: &S`) surfaced them onto the trait, and `PgStateStore`
-- must back them with real storage — a no-op default would silently lose order
-- bodies and break blackout-restore on the native runtime.
--
-- Keyed by `order_id` only (no account scope — the broker order id is globally
-- unique). NO TTL: per-trade lifecycle state that persists until `plan purge`,
-- matching the KV backend's no-`expiration_ttl` write.

CREATE TABLE IF NOT EXISTS order_bodies (
  order_id    text        PRIMARY KEY,   -- broker order id the adapter returned
  signed_body text        NOT NULL       -- the verbatim signed alert body
);
