# TODO — Phase 0: PgStateStore + conformance harness

Branch: `feat/pg-state-store` (worktree, sibling of repo).
DB: `postgresql://candle_cache:candle_cache@localhost:5432/trade_control_dev`
Plan: see `MIGRATION-VM-POSTGRES.md`.

## In progress
- [x] **All 17 state families ported** into `worker/src/pg.rs` — zero `todo!()`,
      builds clean, clippy clean, fmt clean. seen test green vs live DB.
- [ ] **NEXT: Mem-vs-Pg conformance harness** — the real parity proof. Extract
      MemStateStore tests into a generic `conformance<S: StateStore>(store)` and
      run against both. (snapshot is Pg-only: Mem returns empties by design.)
- [ ] Then: `sqlx prepare` (offline), per-family edge tests, commit batches.

### Porting notes for the morning (things to double-check, NOT guessed-final)
- `set_veto`/`mark_retry_fire_seen`/`upsert_*` stamp `expires_at` from
  `Utc::now()` (trait gives no `now`), whereas KV used the worker clock. Fine for
  live, but the offline replay sets a replay clock on MemStore — Pg can't honour
  that yet. If replay ever drives Pg, add a clock injection. Flagged.
- `control_event`: KV keyed on `key_suffix` alone (same suffix overwrote). Pg
  appends with a `seq`. key_suffix embeds set_at epoch, so same-suffix = same
  instant → observationally identical for the audit reader. Verify against the
  `plan show` consumer before calling done.
- Expiry sweep (periodic DELETE) not written yet — reads filter correctly so
  it's correctness-safe; the DELETE job lands with the scheduler (Phase 1).

## Phase 0 checklist
- [x] Fresh `worker` crate (`trade-control-worker`), workspace member, edition 2024
- [x] sqlx added (`postgres,runtime-tokio,tls-rustls,chrono,macros,json,migrate`)
- [x] Schema migration `worker/migrations/0001_state.sql` (17 typed tables) —
      APPROVED, applied to dev DB
- [x] `worker/src/pg.rs` — `PgStateStore` (connect/from_pool/migrate) + full
      `impl StateStore` skeleton; **seen family ported & passing**
- [x] First round-trip test green vs live dev DB (`tests/pg_seen.rs`) —
      caught + fixed an INT4/i64 decode bug
- [ ] Port the other 16 families (cooldown → prep → veto → … → archived_plan)
- [ ] Extract existing MemStateStore `#[test]`s into a generic
      `conformance<S: StateStore>(store)` harness
- [ ] Run conformance against MemStateStore (must still pass — no behaviour change)
- [ ] Run conformance against PgStateStore (parity gate)
- [ ] `sqlx prepare` (offline mode) so other machines/CI build without a live DB
- [ ] clippy + fmt
- [ ] commit + push

## Decisions locked
- Crate: fresh `worker` (NOT in core — keeps core wasm-clean for in-tree CF). Port
  old-worker pieces in as needed, not bulk.
- Schema: 17 flat typed tables, no FKs (hierarchy conventional). account=NULL=global.
- Expiry: lazy filter on read + periodic DELETE (sweep, folded into scheduler later).
- DB: `postgresql://candle_cache:candle_cache@localhost:5432/trade_control_dev`

## State families (→ tables)
seen · cooldown · prep · veto · prep_block · pause · news_window ·
entry_attempt · retry_fire · spread_blackout_window(singleton) ·
blackout_windows(per-instrument) · spread_blackout_record · mw_state ·
trade_plan · plan_state · control_event · archived_plan

## TTL rule (Bug #15 — must preserve)
- **Per-trade rows** (trade_plan, plan_state, archived_plan, entry_attempt,
  control_event): **NO expiry**.
- **Control rows** (cooldown, veto, prep, prep_block, pause, news_window,
  retry_fire, blackout windows/records): carry `expires_at`, filtered on read.

## Open question for user before writing pg.rs
- Sweep strategy for expired rows: lazy-filter-on-read only, or also a periodic
  `DELETE WHERE expires_at <= now()` cleanup? (KV deletes passively on TTL.)
