# TODO — Phase 0: PgStateStore + conformance harness

Branch: `feat/pg-state-store` (worktree, sibling of repo).
DB: `postgresql://candle_cache:candle_cache@localhost:5432/trade_control_dev`
Plan: see `MIGRATION-VM-POSTGRES.md`.

## In progress
- [x] **All 17 state families ported** into `worker/src/pg.rs` — zero `todo!()`,
      builds clean, clippy clean, fmt clean. seen test green vs live DB.
- [x] **Mem-vs-Pg conformance harness — GREEN on BOTH backends.**
      `core::state::conformance::run_all(&store, tag)` (gated `test-support`)
      holds 17 families to one set of assertions. `core` runs it vs
      `MemStateStore` (`conformance_against_memstore`); `worker` runs the same
      vs `PgStateStore` (`tests/pg_conformance.rs`). Both pass.
      → It caught FOUR real parity bugs (all fixed, see below).
- [x] **`sqlx prepare` — N/A by design.** All 67 queries use the *runtime*
      `sqlx::query()` / `query_as()` forms, not the compile-checked `query!`
      macros, so the crate builds offline with no live DB and no `.sqlx/` cache.
      (Don't "upgrade" to `query!` for type-safety — it would reintroduce a
      build-time DB dependency. Runtime queries are validated by the
      conformance test instead.)

### Bugs the conformance harness caught (all fixed)
1. **NULL-account global rows were unstorable.** 11 tables put `account` in a
   PRIMARY KEY → implicitly NOT NULL → every global (`None`) row rejected.
   Fixed: dropped `account` from those PKs, replaced with
   `UNIQUE INDEX (COALESCE(account,''), …)`. Dev DB schema reset to re-apply.
2. **TTL clamp mismatch.** Mem/KV floors a tiny ttl to `MIN_TTL_SECONDS` (60s)
   on the control families; Pg didn't → a ttl-0 control write expired instantly
   on Pg vs lived 60s on Mem. Fixed: `control_expires_at()` floor on all 11
   clamped Pg families (NOT `seen` / the no-TTL per-trade tables).
3. **Timestamp precision.** Pg `timestamptz` truncates to µs; Mem/KV keep ns.
   An audit (3 agents) confirmed NO worker comparison is finer than µs
   (bar/signal times are second-granular; `incoming.rs:251` freshness is
   hour-granular) → µs is safe. Fixed in the *harness only* via `now_us()`
   (µs-aligned inputs) — no production code change; the contract under test is
   "same instant to storage precision".
4. **`ON CONFLICT (account, …)` broke** after the PK change (the two upserts
   that used it — `record_entry_attempt`, `archive_plan`). Fixed to
   `ON CONFLICT (COALESCE(account,''), …)` (expression-index inference). The
   other account-scoped writers already use NULL-safe DELETE-then-INSERT.

### Remaining (Phase 0 tail)
- [ ] Snapshot test (Pg-only — Mem returns empties by design).
- [ ] Per-family negative/edge tests beyond the conformance core, if any gaps.
- [ ] Commit batches (this checkpoint = harness + 4 fixes).

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
- **Schema was edited after first apply.** `0001_state.sql` changed (PK→unique
  index on 11 tables). The dev DB was reset to re-apply: the trade-control
  tables + `_sqlx_migrations` were dropped and `migrate()` re-ran fresh. Pre-prod,
  no data lost. **If you build against a *different* Postgres that already ran
  the OLD `0001`, sqlx will hit a checksum mismatch** — drop those 17 tables +
  the `_sqlx_migrations` row and let `migrate()` re-apply (or, once we ship,
  fold the change into a real `0002` migration instead of editing `0001`).
  For now `0001` is the single source and has never left dev.

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
