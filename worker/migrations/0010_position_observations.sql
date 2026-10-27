-- Reporting is independent of expiring retry attempts and plan rule phase.
CREATE TABLE position_observations (
    account TEXT NOT NULL,
    position_id TEXT NOT NULL,
    trade_id TEXT NOT NULL,
    order_id TEXT NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL,
    missing BOOLEAN NOT NULL DEFAULT FALSE,
    closed_exit JSONB,
    body JSONB NOT NULL,
    PRIMARY KEY (account, position_id)
);
CREATE INDEX position_observations_trade ON position_observations (trade_id);
CREATE TABLE position_sync (
    account TEXT PRIMARY KEY,
    checked_at TIMESTAMPTZ NOT NULL,
    error TEXT
);
