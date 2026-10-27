-- Every accepted placement, including engine-latched single-shot entries.
-- This is reporting identity, not another retry slot or a plan-phase change.
CREATE TABLE position_links (
    account TEXT NOT NULL,
    order_id TEXT NOT NULL,
    trade_id TEXT NOT NULL,
    body JSONB NOT NULL,
    resolved BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (account, order_id)
);
CREATE INDEX position_links_trade ON position_links (trade_id);
