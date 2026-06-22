# Fix: `plan show` 404s on archived plans — DONE

Branch: `fix/plan-show-archived` (worktree). Target env: **dev** (`main`).

## Bug
`trade-control-dev plan list --include-archived` lists an archived (terminated)
plan, but `plan show <trade_id>` returned 404 for it. `handle_plan_show` only
scanned live plans via `list_all_trade_plans`; it never consulted the archive
(unlike `handle_plan_delete`, which scans both).

## Done
- [x] Added `archived_at: Option<DateTime<Utc>>` to `PlanDetail` (mirrors
      `PlanSummary`); only emitted for archived matches.
- [x] Factored pure, `StateStore`-generic helper `collect_plan_details(store,
      target)` scanning live then archived plans. `worker::Response` stays in
      `handle_plan_show`.
- [x] `handle_plan_show` calls the helper; 404 only when both empty.
- [x] Unit tests (`plan_show_tests`, using core `MemStateStore` via new
      `test-support` dev-dependency): archived-only found + flagged, live found
      + not flagged, unknown id empty.
- [x] README `plan-show` bullet updated; CHANGELOG v50 added.
- [x] cargo test (227 ok) / clippy (native + wasm) / fmt all green.

## Remaining
- [ ] Commit + push + tag v50 + advance parent gitlink.
- [ ] Deploy to dev (`./deploy-dev.sh`) so the live `-dev` worker picks it up,
      then re-verify `plan show hs-nzd-chf-d12eb831` against it.
