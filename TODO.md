# TODO — native runtime migration (CF Worker → VM + Postgres)

Branch: `feat/native-runtime` (off `feat/pg-state-store`); worktree sibling.
DB: `postgresql://candle_cache:candle_cache@localhost:5432/trade_control_dev`
Plan: see `MIGRATION-VM-POSTGRES.md` → "Phase 1 — DECIDED".

## Phase 1 — native runtime (IN PROGRESS)

Goal: a native binary (`trade-control-worker`) that replaces the Cloudflare
Worker entirely. axum HTTP receiver + per-task tokio scheduler, backed by
`PgStateStore`, brokers built from the enc account store + env secrets.

### Build order (each step green before the next)
- [ ] **Map the dispatch surface** (Explore agent on `src/lib.rs`): run_action
      chain, every `worker::Response` site, broker factory, `open_store`,
      secrets read off `Env`, recording sink. → seam map.
- [ ] **Broker factory (native).** Replace `&worker::Env` secret-reads with a
      native config: `broker-oanda::login(env)` + TN login currently take
      `&worker::Env`. Build a `BrokerFactory` that constructs per-account
      brokers from `~/.config/tradenation/accounts.enc` + env secrets
      (`OANDA_API_KEY`, signing key, …). Non-wasm `tradenation-api` (CLI's dep).
- [ ] **Config loader.** `config.toml` (bind addr/port, DB URL, intervals) +
      env-secret resolver. `serde`/`toml`.
- [ ] **axum receiver.** Lift `run_action` off `worker::Response` →
      `IntoResponse`. Reuse `core` verify/parse/gates UNCHANGED. One POST route
      for the webhook; bind `127.0.0.1:PORT` (reverse proxy terminates TLS).
      Graceful SIGTERM shutdown.
- [ ] **Per-task scheduler.** Port `src/cron/*` one module at a time, each
      taking `&PgStateStore` + `&BrokerFactory` instead of `&Env`. Own tokio
      `interval` per task at its natural cadence (engine fast, session slow,
      blackout-hours daily). Order: session_refresh → sweep → blackout_watch →
      breakeven_watch → engine → blackout_apply → blackout_hours. Plus the
      expiry-sweep DELETE job (Phase 0 deferred this here).
- [ ] **Recording sink → Postgres.** `recordings(kind, body jsonb, …)` table;
      port `recording.rs` + `tick_recording.rs` R2 writes to inserts.
- [ ] **Parity gate.** Run the replay/tick-bundle harness against the native
      binary; diff decisions vs the CF worker on the same recorded inputs.

### Dispatch surface map (Explore agent, 2026-06-29) — KEY SEAMS
- **fetch entry**: `#[event(fetch)] async fn main(req, env, ctx)` `src/lib.rs:107`.
  Opens store inline (`env.kv("TRADE_CONTROL_KV")` → `KvStateStore::new`,
  `src/lib.rs:192`), acquires broker, calls `run_action`.
- **`run_action<B: Broker>(broker, store: &KvStateStore, verified, env, now, raw)`**
  `src/lib.rs:537`. Branches on `verified.intent.action`: Enter→run_enter,
  Close→run_close, Invalidate→run_invalidate, escalated-Veto→run_veto_with_broker.
  Non-broker actions handled in `main` BEFORE broker dispatch.
- **`ActionResult::Rejected { response: Result<Response>, outcome }`**
  `src/lib.rs:514` — embeds a `worker::Response` (panics off-wasm at
  construction). **CENTRAL REFACTOR**: carry status+body DATA, build
  `IntoResponse` at the axum edge.
- **No `open_store` fn** — store built inline; just inject `&PgStateStore`.
- **Brokers** `acquire_oanda_broker` `:3521` / `acquire_tn_broker` `:3588`
  (named-account paths `:3533`/`:3663` are wasm-only; native stubs return None).
  Named-account routing reads `AccountMetadata` from the `accounts:index` KV
  blob + per-account `TN_ACCOUNT_*`/`OANDA_ACCOUNT_*` secrets.
- **Secrets** (all `env.secret`, no `env.var`): `SIGNING_KEY` (HMAC + diag key),
  `MAX_RISK_PCT_PER_TRADE` (def 1.0), `MAX_OPEN_POSITIONS` (def 3),
  `PIP_SIZE_<INSTR>` (per-instr override, fallback 0.0001), `ADMIN_KEY`
  (`X-Admin-Key`), `OANDA_API_KEY`/`OANDA_ACCOUNT_ID`/`OANDA_LIVE`,
  `TN_ACCOUNT_<NAME>`/`OANDA_ACCOUNT_<NAME>` (JSON cred blobs).
- **Recording**: `record_to_r2` (`src/recording.rs`, prefix `req/`) +
  `record_tick_to_r2` (`src/tick_recording.rs`, prefix `ticks/`), both
  fire-and-forget via `ctx.wait_until`, wasm-only with native stubs. → two
  Postgres tables (`request_records`, `tick_bundles`).
- **Native stubs already present** (the port targets): broker named-account
  acquires, `load_account_caps` `:3616`, both R2 recorders.

### Phase 1 hazards / decisions
- Broker ADAPTERS are already `Env`-free (`OandaBroker`/`TradeNationAdapter`
  hold a plain client/session). Only the LOGIN helpers read `Env` secrets.
  And `tn_login.rs` (wasm redirect-chain hack) is UNNEEDED off-wasm — its own
  header says it reimplements `tradenation_api`'s native login. So the native
  TN path is `tradenation_api::login_demo_named(account)` (reads the enc store),
  exactly as the CLI/MCP already do.
- **ACCOUNT METADATA — DECIDED (2026-06-29): Postgres `accounts` table.**
  KV held `AccountMetadata` {name, broker, kind, caps, oanda_account_id} in one
  `accounts:index` blob. Native: an `accounts` table (name PK, broker, kind,
  oanda_account_id, caps jsonb), with a `PgMetadataStore: MetadataStore` impl
  alongside `PgStateStore`. Managed via the ported `account add`/`remove` admin
  route. TN login creds still resolved from the enc store by name; OANDA api-key
  from env. Keeps add/remove dynamic at runtime; consolidates state in Postgres.
- `thread_local!` log buffer assumes one-request-per-thread (TRUE on Workers,
  FALSE under multithreaded tokio) — `src/recording.rs:18`. Must become
  request-scoped capture (tracing layer / request-extensions) or concurrent
  requests cross-contaminate captured logs.
- `ctx.wait_until` (fire-and-forget recording) → `tokio::spawn`.
- KV `index:*` secondary-index machinery → delete entirely; use SQL
  `WHERE expires_at > now()` (snapshot test already does this).
- `console_log!` / `rlog!` / `rlog_err!` → `tracing::info!` / `error!`.
- `tracing_console::ConsoleSubscriber` → ordinary `tracing_subscriber` fmt layer.

---

# TODO — Phase 0: PgStateStore + conformance harness  (DONE, on `feat/pg-state-store`)

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
