-- Account metadata index — the native replacement for the Cloudflare KV
-- `accounts:index` JSON blob (`src/accounts/kv_metadata.rs`).
--
-- Holds the non-secret `AccountMetadata` for every named account: which broker
-- it routes to, demo vs live, optional per-account risk caps, and (for OANDA)
-- the sub-account id under the shared API key. Credentials themselves are NOT
-- here — TN login creds live in the enc store (`~/.config/tradenation/accounts.enc`,
-- resolved by name), the OANDA API key in an env var.
--
-- Typed columns (not a single jsonb body) so the dispatch can query by broker
-- (`WHERE broker = 'oanda'`) and the operator-facing `account list` is a plain
-- ordered SELECT. `broker` / `kind` store the lowercase serde form of
-- `BrokerKind` / `AccountKind` (`oanda`|`tradenation`, `demo`|`live`); `caps`
-- is the serde shape of `AccountCaps` (`{}` when default).

CREATE TABLE IF NOT EXISTS accounts (
  name             text        PRIMARY KEY,        -- operator-chosen, kebab-case, unique
  broker           text        NOT NULL,           -- BrokerKind serde: 'oanda' | 'tradenation'
  kind             text        NOT NULL,           -- AccountKind serde: 'demo' | 'live'
  oanda_account_id text,                           -- required iff broker = 'oanda'; NULL for TN
  caps             jsonb       NOT NULL DEFAULT '{}'::jsonb  -- AccountCaps (default = {})
);
