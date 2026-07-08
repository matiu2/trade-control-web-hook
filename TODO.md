# TODO — Fix OANDA PRICE_PRECISION_EXCEEDED (order not rounded to tick) — IN PROGRESS 2026-07-08

Incident: `hs-au200-aud-b89106ee` QM entry fired correctly (r=2.195) but OANDA
rejected it (`PRICE_PRECISION_EXCEEDED`) — worker sent 5-dp price on a 0.1-tick
instrument. Same rejection hit XAU_USD, AUD_JPY same tick → systemic. Missed a
winning trade (price ran to TP, then `01-veto-too-low` archived the plan).

## Bug B — instrument-lookup catalog (separate repo: ../../instrument-lookup)
- [ ] `AU200AUD` duplicate entry has `tick_size 1.0 / dp 0`; canonical `AU200`
      correctly has `0.1 / 1`. Fix the duplicate to match (or dedupe). tv-arm
      resolves via the OANDA-derived `AU200AUD` id → got the wrong tick.
- [ ] Test + `cargo install --path .` so tv-arm picks up the fix.
- [ ] Audit other OANDA index entries for wrong tick (follow-up).

## Bug A — worker never rounds order price/SL/TP to instrument tick — PR-1 DONE
- [x] `core/src/rounding.rs`: `round_to_tick`/`round_price`/`round_stop_loss`/
      `round_take_profit` (identity on tick<=0; directional SL/TP). 10 tests.
- [x] `Intent.tick_size: Option<f64>` (auto-signed line-scan HMAC) +
      `DispatchConfig.tick_size`. incoming.rs sign/round-trip/tamper tests.
      (EntryAttempt/TradePlan intentionally NOT added — intent field is the
      authoritative source both worker+replay read; would be dead/churny.)
- [x] Resolver: `tick_size` param on from_intent/from_mw_intent/
      finish_with_sizing; rounds entry/SL/TP BEFORE in-range + R-floor checks.
      Tests: AU200 8806.70784→8806.7; sub-1R-after-rounding rejected.
- [x] Worker edge (enter.rs:284): tick = intent.tick_size → cfg.tick_size →
      pip_size (fail-open). Threaded to from_intent.
- [x] Replay/engine parity: simulator.rs `replay_tick`, evaluate.rs, report.rs
      `replay_report_tick`, replay.rs DispatchConfig, blackout_restore.rs — all
      round with intent.tick_size→pip fallback (matches worker).
- [x] tv-arm bakes `asset.tick_size` onto H&S + M/W + position enters
      (--tick-size override); TradeSpec/MwSpec/PositionEnterSpec carry it.
- [x] clippy clean (1 pre-existing spread_blackout warning untouched) + fmt.
- [x] All 24 live-crate test suites green.

## Verify (Task #6) — DONE
- [x] Replayed the REAL `hs-au200-aud-b89106ee` plan (exported from staging) over
      the incident window (OANDA candles, 2026-07-07→09). With tick 0.1 baked the
      enter resolves `entry=8806.7 sl=8841.4 tp=8730.6` (the exact price OANDA
      rejected at 8806.70784), PLACES, FILLS @ 8806.7, and TAKES PROFIT → +2.19R
      (+$2,193 on $100k). Unfixed (no baked tick → pip fallback 1.0) resolves
      8807.0 and also places (any grid beats the raw 5-dp reject). Confirms the
      fix recovers the missed winner. instrument-lookup resolve AU200_AUD → AU200
      row (tick 0.1) wins first-match, so tv-arm already bakes 0.1 for AU200 →
      PR-1 fixes it without the PR-2/3 catalog work.

---

# TODO — spread-window SL floor (mean spread over trailing N candles) — IN PROGRESS 2026-07-06

## Problem

The System-1 entry SL-spread floor sizes the "10× spread" floor off a **single**
spread sample:
- **Worker** (`run_enter`): one live `get_quote` at entry time.
- **Replayer** (`apply_entry_spread_floor`): the fire bar's `ask_c − bid_c`.

Entry candles are often spiky (high volatility → wide spread on that exact bar),
so a one-off spread spike blows the 10× floor out and widens the stop too far.

## Fix

Size the floor off the **mean** `(ask − bid)` over the trailing **N** candles
(including the entry bar), where **N is tunable at trade-design time**, default
**5**. Keeps the protective floor; stops one spiky bar from dominating it.
System 2 (spread-hour transient widen) is **unchanged**.

## Design decisions (from operator)
- Mean (not median/max) of the last N candles' close spread `(ask_c − bid_c)`.
- N tunable per-trade, default 5.
- Worker + replayer compute the **same** average (shared core helper).
- `pip_size`-free: the floor stays a pure ratio of price distances.

## Steps (each ≤600 lines, tests green before moving on)
- [x] **1. core: `mean_spread` helper + signed `spread_window` field + default const.**
- [x] **2. core/broker: `get_bidask_candles` trait method (default-impl'd).**
- [x] **3. broker-oanda: real `get_bidask_candles` (keep the MBA bid/ask).**
- [x] **4. broker-tradenation-adapter: real `get_bidask_candles`.**
- [x] **5. worker `run_enter`: use windowed mean spread (fail-open to get_quote).**
- [x] **6. replayer `apply_entry_spread_floor`: use windowed mean spread.**
- [x] **7. tv-arm `--spread-window N` → bake onto signed intent (+ build-trade).**
- [x] **8. README + CHANGELOG vNN + memory; clippy+fmt; commit/push/parent-bump.**

## Hazards to preserve
- `strategy_changes_in_both_replayer_and_worker` — steps 5 & 6 land together.
- Signed-body scan is top-level single-line only.
- Mid `get_candles` contract is load-bearing for the engine — don't touch it.
- Fail-open on the worker floor (quote/candle error must not strand a legit entry).

---

# TODO — time-decaying retest tolerance (IN PROGRESS 2026-07-03)

Loosen the retest intrabar cross so its closeness-to-neckline requirement
**decays as bars pass** since the break-and-close.

## Spec (operator, 2026-07-03)

`N` = bars since break-and-close (first bar after break = `N=1`), counted in
`detector_window` with `time > break_close_at`, up to & incl. the current bar.

```
atr = wilder_atr(detector_window, atr_length_for(plan.granularity))  // hard-fail if None (unreachable at retest phase)
tol = (N - 1) × plan.retest_atr_step × atr                            // N=1 → 0
```

- Bar 1: `tol = 0` → wick must **reach the line**.
- Each later bar: `+ retest_atr_step · ATR` of **near-side** slack.
- `retest_atr_step` default **0.075** (~1 ATR of slack by bar ~14).

"Within tol" loosens the retest side (long/`Down`: `low <= line + tol`;
short/`Up`: `high >= line - tol`), replacing "must reach/pierce". Only the
retest uses it; `cross_buffer_pct` still governs other intrabar consumers.

## Tasks
- [x] core: `TradePlan.retest_atr_step: f64`, serde-default 0.075 (signed)
- [x] engine: tolerance-aware retest cross in `stamp_retest` (count N, wilder_atr,
      near-side tolerance in `retest_crossed`; hard-fail on None ATR)
- [x] engine tests: N=1 must-reach; N>1 near-miss-within-tol fires; beyond rejects;
      tolerance-grows-linearly (4 new)
- [x] tv-arm: `--retest-atr-step` flag bakes the field (+ test)
- [x] parity: shared engine → worker + replay follow
- [ ] README + CLAUDE.md + CHANGELOG; clippy+fmt; commit/push/parent-bump;
      deploy-dev + rebuild native worker
- [ ] (later, operator) visually tune `retest_atr_step`

---

# TODO — native runtime migration (CF Worker → VM + Postgres)

Branch: `feat/native-runtime` (off `feat/pg-state-store`); worktree sibling.
DB: `postgresql://candle_cache:candle_cache@localhost:5432/trade_control_dev`
Plan: see `MIGRATION-VM-POSTGRES.md` → "Phase 1 — DECIDED".

## Phase 1 — native runtime (IN PROGRESS)

Goal: a native binary (`trade-control-worker`) that replaces the Cloudflare
Worker entirely. axum HTTP receiver + per-task tokio scheduler, backed by
`PgStateStore`, brokers built from the enc account store + env secrets.

### Build order (each step green before the next)
- [x] **Map the dispatch surface** (Explore agent) → seam map below.
- [x] **Broker factory (native).** `worker/src/broker_factory.rs`:
      `acquire_oanda(meta, secrets)` + `acquire_tn(meta)`. Plus the
      `broker-tradenation-adapter` crate extraction + `OandaBroker::from_api_key`
      + `PgMetadataStore` (account index). DONE (commits 86cb62a→0b19192).
- [x] **Config loader.** `worker/src/config.rs` (TOML) + `secrets.rs` (env). DONE.
- [ ] **Dispatch extraction → shared crate** (user DECIDED 2026-06-29).
      Move `run_action` + `run_enter`/`run_close`/`run_invalidate`/
      `run_veto_with_broker` + `handle_*` control handlers out of the root wasm
      `src/lib.rs` into a shared crate (lean: a new `dispatch` crate, or `core`),
      made generic over `<S: StateStore, B: Broker>` and returning a worker-FREE
      `DispatchResult { status: u16, body: String, outcome: String }` (replacing
      `ActionResult::Rejected { response: worker::Response }`). The wasm worker
      maps `DispatchResult → worker::Response` at its edge; the native crate maps
      it to axum `IntoResponse`. ONE dispatch, no drift. Staged as 4 increments,
      each keeping the wasm worker green:
      - [x] **(7) de-worker `ActionResult`** — `Rejected` carries
            `{status: u16, body: String, outcome}`; `worker::Response` built only
            at the single fetch-path consumption edge. Commit `957420b`.
      - [x] **(8a) StateStore axis** — the 22 dispatch fns `store: &KvStateStore`
            → `store: &S` with a `<S: StateStore>` bound. Commit `0f99518`.
            Surfaced + fixed an order-body parity gap (put/get/delete_order_body
            were inherent to KvStateStore, off the trait → PgStateStore missed
            them). Now on the trait (no-op defaults for test stores), real Mem +
            Pg impls, `order_bodies` table (migration 0004), and an `order_body`
            conformance family guarding it on BOTH backends.
      - [x] **(8b) env axis → `DispatchConfig`** — `run_enter` (+ `run_action`)
            were the only dispatch fns reading `env` for config (4 reads:
            `secret_or_default` ×2, `pip_size_for`, `load_account_caps`).
            Replaced `env: &Env` with a pre-resolved
            `core::dispatch_config::DispatchConfig { worker_max_risk_pct,
            worker_max_open_positions, pip_size, caps }`, built at the EDGE by
            `build_dispatch_config(env, verified)` (wasm) — the 3 call sites
            (fetch handler, cron engine, blackout_restore) build it then pass
            `&cfg`. The native runtime builds the same struct from `Secrets` +
            `PgMetadataStore`. The `r2_purge` env uses (plan-purge / older-than)
            stay `env: &Env` — they're the *recording* backend (R2 → Postgres
            `tick_bundles` is Task #6), NOT state, handled with the recording
            sink. Entry dispatch path is now fully `Env`-free + `StateStore`-
            generic → ready to relocate to `core` (increment 9).
      - [x] **(9-relocate) ActionResult dispatch → core::dispatch** (commit
            4e8abcb) — the 5 ActionResult fns + ActionResult enum + helper
            closure moved into `core/src/dispatch/` (action/enter/close/
            invalidate/veto/action_result/control_result/shared). rlog!→tracing!.
            wasm worker re-exports them (kept compiling — DEAD END, not polished).
            4 dead re-export shims deleted. core 730 tests, all consumers build.
      - [ ] **(9-replay) re-point replay at the REAL run_enter** (IN PROGRESS,
            agent a3dbc0e) — USER PRIORITY: replay behaviour must match the engine
            as closely as possible. The replay (cli/src/bin/replay_candles) already
            has a MemStateStore+clock + ReplayBroker (full Broker impl) and calls
            pause_gate/retry_gate piecemeal but MISSES cooldown/prep/veto/
            entry-level-veto. Replace the hand-assembled gates with ONE
            run_enter(&ReplayBroker, &store, &verified, &cfg, ...) call, carry the
            ActionResult outcome on Fire (EnterGateOutcome), report reads it
            instead of re-deriving via engine::entry_gate_block (DELETE that
            mirror). Maximum fidelity, kills the drift class.
      - [ ] **(9-control) de-worker + relocate the 17 control handlers** — convert
            handle_status/prep/veto/pause/resume/register/plan_* from
            `Result<Response>` → `core::dispatch::ControlResult{status,body}` (like
            #7 did ActionResult: Response::ok(b)→ControlResult::ok(b),
            Response::error(m,s)→ControlResult::error(m,s); the edge maps back via
            is_success). Then relocate to core::dispatch. Worker + native map
            ControlResult→their response at the edge. User chose "move EVERYTHING".
            CATALOG (post-relocation line nums, ~63 Response sites total):
              env-FREE (15, fully relocatable): handle_status(L474),
              handle_unlock(523), handle_prep(557), handle_prep_expire(689),
              handle_veto(735), handle_clear_prep(822), handle_clear_veto(870),
              handle_pause(915), handle_resume(966), handle_news_start(998),
              handle_news_end(1047), handle_register(1087), handle_plan_list(1191),
              handle_plan_show(1351), handle_plan_delete(1401).
              env-USING (2, convert return type but KEEP env + stay in worker until
              Task #6 R2→PG): handle_plan_purge(1493, r2_purge), 
              handle_purge_older_than(1594, r2_purge).
            Edge consumers: the 17 `break 'intent handle_*` sites in main's fetch
            loop wrap with a control→Response mapper.
- [ ] **axum receiver.** Once dispatch is worker-free + generic: one POST route,
      verify/parse via `core`, call the shared `run_action::<PgStateStore, _>`,
      map `DispatchResult` to `IntoResponse`. Bind `127.0.0.1:PORT` (proxy TLS).
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
