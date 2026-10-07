-- One confirmed closure per entry order, independent of cron cadence or a
-- worker restart. Legacy warning notes have no broker_exit and stay intact.
CREATE UNIQUE INDEX IF NOT EXISTS cron_notes_broker_exit_key
  ON cron_notes (COALESCE(account, ''), correlation_id,
                 (body->'broker_exit'->>'broker_order_id'))
  WHERE body->'broker_exit'->>'broker_order_id' IS NOT NULL;
