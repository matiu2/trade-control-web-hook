# Operator plan transitions

The journal controls call signed `trade-control` commands. Use the staging
wrapper or the appropriate `--endpoint` when managing staging plans:

```sh
trade-control-staging plan paper TRADE_ID --account reversals --detach-existing
trade-control-staging plan swap-account TRADE_ID --account reversals --detach-existing
trade-control-staging plan closed-manually TRADE_ID
trade-control-accounts list --json
```

`paper` chooses a separate account for market data and logs paper entry signals.
It uses the plan's observe-only flag and does not simulate fills, positions or
P&L. The data account must have working credentials; the picker lists metadata
without testing credentials. Choosing a broker account whose kind is `demo`
alone still permits broker demo execution.

`swap-account` changes future execution and market data. It preserves the
plan's existing paper setting. Existing broker positions and pending orders
are detached, never transferred or closed. `--detach-existing` explicitly
acknowledges that distinction. Both controls require a different account.

The server maps known FX instruments through the instrument catalog for a
cross-broker switch, updates every embedded rule's broker/account/symbol and
precision, and rejects unknown instruments or differing pip units. Cross-broker
fixed size and contract multiplier, and any fixed monetary risk whose target
account currency is unverified, require explicit re-arming instead. Same-broker
switches retain their instrument and sizing units.

A switch retains rule IDs, completed prep steps and engine phase. It resets the
signal clock to the operation time and clears prior feed price comparisons.
The bar forming during the switch is skipped; only bars opened afterward can
produce new price signals. It does not recover signals missed while the old
account was unavailable.

`closed-manually` accepts the operator's assertion immediately, archives the
plan with `operator:closed-manually`, and removes its local management records.
It contacts no broker. A separate best-effort history audit can corroborate the
closure later; unavailable history or expired credentials do not reactivate
management or invalidate the operator assertion.

All transitions wait for in-flight engine/upkeep jobs and commit the plan,
management detach and audit snapshot in one Postgres transaction. The audit is
a `cron_notes` row with `source = 'plan-admin'`; `body.admin_snapshot` retains the
original plan/state, detached entry attempts, hold record and signed order
bodies. Manual closure begins with `audit_status = 'pending'` for the history
worker. Incoming old signed broker requests or registrations are rejected after
a paper/account/manual-closure transition. Explicit deletion is required before
re-registering a plan whose account or mode changed.
