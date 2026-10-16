-- Committed with its journal observation; delivery retries survive restarts.
CREATE TABLE operator_alert_outbox (
    event_key text PRIMARY KEY,
    body jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_attempt_at timestamptz,
    delivered_at timestamptz
);
CREATE INDEX operator_alert_outbox_pending ON operator_alert_outbox (last_attempt_at NULLS FIRST, created_at)
    WHERE delivered_at IS NULL;
