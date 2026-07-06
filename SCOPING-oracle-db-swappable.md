# Scoping: Oracle DB as a swappable backend (alongside Postgres)

**Status:** scoping only — no code written.
**Question asked:** "Could you scope out having Oracle DB as the backend swappable with Postgres?"
**Date:** 2026-07-06
**Verdict up front:** the *store abstraction* is already clean and swap-ready; the
*driver* is the whole problem. `sqlx` (the DB layer the native worker is built on)
**has no Oracle driver**, so an Oracle backend is not "add one more impl" — it is a
parallel driver stack plus a non-trivial SQL-dialect port. Estimate below.

---

## 1. What's already good (the cheap half)

The persistence contract is a **trait**, not a concrete type, and it already has two
impls — so a third backend is a *known-supported* shape, not a green-field design.

- **`core::state::StateStore`** — `core/src/state.rs:560+`. ~50 async methods
  (`is_seen`/`mark_seen`, prep/veto/cooldown/pause/news set+get+clear, entry-attempt
  CRUD, spread-blackout records, trade-plan + plan-state + control-event + archived-plan
  + order-body CRUD). Return types are `impl Future<...>` (RPITIT native-async).
- **Second impl already exists:** `MemStateStore` (in-memory, `core/src/state.rs`
  test module) — used across `pause_gate`, `retry_gate` tests. Proof the trait is
  genuinely backend-agnostic.
- **Consumers are generic `<S: StateStore>`** (`core::dispatch`, `pause_gate`,
  `retry_gate`, `trade-control-cron`), never `&dyn`. The shared cron engine and
  gates would bind an `OracleStateStore` with **zero changes** — they only know the
  trait.
- **Account metadata:** `core::account` documents a parallel `MetadataStore` shape;
  native impl is `PgMetadataStore` (`worker/src/pg_accounts.rs`).

**So a new backend = one new crate/module implementing `StateStore` (+ metadata +
recording), and the entire gate/engine/dispatch layer picks it up for free.**

---

## 2. What's hard (the expensive half)

### 2a. There is no `sqlx` Oracle driver — this is the load-bearing fact

`worker/Cargo.toml`: `sqlx = { features = ["postgres", ...] }`. sqlx supports
Postgres / MySQL / SQLite / MSSQL only. **Oracle is not and will not be a sqlx
backend.** So `OracleStateStore` cannot reuse *any* of `pg.rs`'s query plumbing —
it needs a different driver entirely. Realistic Rust options for Oracle:

| Option | Notes |
|---|---|
| **`oracle` crate** (ODPI-C bindings) | Most mature. **Not async** — blocking calls; must wrap in `tokio::task::spawn_blocking`. Requires Oracle Instant Client (`.so`) on the VM + at build time. |
| **`sibyl`** | Async-ish, also ODPI-C / OCI based, less widely used. |
| **Oracle REST (ORDS) over HTTP** | Avoids the native client but adds a network hop + JSON marshalling for every query; poor fit for a hot path. |

All three mean: **new dependency, new connection-pool type, new bind syntax,
new error mapping** — none of `pg.rs` transfers mechanically. And the mature option
is *blocking*, which fights the tokio/axum async worker (every store call becomes a
`spawn_blocking` round-trip).

### 2b. The SQL is Postgres-dialect, in 42 runtime query strings

Good news first: **zero `sqlx::query!`/`query_as!` compile-time macros** — all
**42** queries are runtime `sqlx::query("...")` / `query_as("...")` with **161
`.bind()`** calls (`worker/src/pg.rs`). So there's **no `.sqlx` offline cache / live-DB-at-compile-time**
requirement to untangle. But the SQL strings themselves are Postgres-flavoured and
must be rewritten for Oracle:

| Postgres-ism | Count / where | Oracle equivalent |
|---|---|---|
| `ON CONFLICT ... DO UPDATE` (upsert) | **11** in `pg.rs` | `MERGE INTO ... WHEN MATCHED/NOT MATCHED` — a full rewrite per statement |
| `$1..$N` positional params | 6 distinct, 161 binds | Oracle uses `:1`/`:name`; the `oracle` crate binds differently from sqlx |
| `jsonb` columns + operators | **31** in `pg.rs`, ~16 in migrations | Oracle `JSON` type (21c+) or `CLOB` + `IS JSON`; different accessors, no `::jsonb` cast, no `->>`/`@>` |
| `timestamptz` | **24** in migrations | `TIMESTAMP WITH TIME ZONE` — close, but `now()` → `SYS_EXTRACT_UTC(SYSTIMESTAMP)` |
| `bigserial` / `serial` PKs | 4 / 13 | `GENERATED ALWAYS AS IDENTITY` (12c+) or a sequence+trigger |
| `RETURNING` | 2 | `RETURNING ... INTO :out` (different call shape) |
| `IS NOT DISTINCT FROM` (NULL-safe eq) | **25** in `pg.rs` | Postgres-only; Oracle needs `DECODE()`/`CASE` — used because `account` is nullable for global rows |
| **partial indexes** `... WHERE trade_id IS NOT NULL` | `0003_recordings.sql` | Oracle has no partial index — needs a function-based index or drop the optimisation |
| `CREATE UNIQUE INDEX ... COALESCE(account,'')` | several in `0001` | function-based unique index; Oracle supports it but syntax differs |
| `'{}'::jsonb` default | `0002_accounts.sql` | `'{}'` into a JSON/CLOB column, no cast operator |
| TTL: `WHERE expires_at > now()` + `gc_expired` `DELETE ... WHERE expires_at < now()` | `pg.rs:112,135,173...` | `now()` → `SYS_EXTRACT_UTC(SYSTIMESTAMP)`; logic identical |

**All 4 migrations** (`0001_state` … `0004_order_bodies`) are Postgres DDL and need an
Oracle-dialect parallel set. sqlx's own `migrate` feature is Postgres-bound, so
migrations would run via a different mechanism (raw script, or the `oracle` crate).

### 2c. The app layer names `PgStateStore` concretely — swap point #2

The gate/engine layer is generic and free. But the **worker app layer hardcodes the
concrete type**, so "swappable at startup" needs one of two changes:

- `worker/src/main.rs:59` — `PgStateStore::connect(&config.database.url)`
- `worker/src/http.rs:74-75` — `AppState { store: PgStateStore, accounts: PgMetadataStore }`
  (concrete fields; every handler reads `state.store` as that concrete type)
- `worker/src/scheduler.rs`, `worker/src/native_cron.rs` — take `PgStateStore` directly
  (some methods like `gc_expired` are inherent to `PgStateStore`, **not** on the trait —
  those need to move onto a trait or be duplicated).
- **`.pool()` leaks:** `worker/src/http.rs:343` `handle_plan_timeline(store: &PgStateStore, ...)`
  and `native_cron.rs:121` reach into `store.pool()` for the recording reads/writes (which
  bypass the trait entirely — see 2d). Every `.pool()` caller is a hard Postgres dependency
  that a backend swap must re-route.
- **`pg_accounts.rs:68`** `row_to_metadata(row: &sqlx::postgres::PgRow)` names the concrete
  Postgres row type — Oracle's driver row type is different, so this must be re-written, not
  re-impl'd.

Two ways to make it runtime-swappable:

1. **Genericise the app layer** — `AppState<S: StateStore, M: MetadataStore>`, thread
   the generic through axum handlers + scheduler. Cleanest, but touches every handler
   signature; axum `State<T>` extractor makes this verbose.
2. **Enum wrapper** — `enum AnyStore { Pg(PgStateStore), Oracle(OracleStateStore) }`
   that impls `StateStore` by delegating each of the ~50 methods. Because the trait is
   RPITIT (`impl Future`), it is **not object-safe** — you *cannot* use
   `Box<dyn StateStore>`. Enum-dispatch is the pragmatic route; ~50 one-line delegations
   (a macro helps). Config picks the variant at boot from `config.database` (add a
   `backend = "postgres" | "oracle"` field + Oracle DSN).

### 2d. Recording layer — direct sqlx, no trait

`worker/src/recording_pg.rs` writes `request_records` / `tick_bundles` with bare
`sqlx::query` (no abstraction). For Oracle this needs its own parallel module.
Small (~2 tables, insert + a couple of selects) but not free.

---

## 3. Effort estimate

Assuming the goal is "run the native worker against *either* Postgres or Oracle,
chosen by config":

| Work item | Rough size |
|---|---|
| Add `oracle` crate + connection pool + Instant Client build/runtime setup on the VM | S–M (infra-heavy: native `.so`, env, CI) |
| `OracleStateStore` impl of ~50 `StateStore` methods (MERGE rewrites, JSON, binds) | **L** — this is the bulk |
| Oracle-dialect migrations (4 files → Oracle DDL, run via non-sqlx path) | M |
| `OracleMetadataStore` + Oracle recording module | S–M |
| Runtime backend selection (enum wrapper `AnyStore` + config field) OR genericise `AppState` | M |
| Move `PgStateStore`-inherent methods (`gc_expired`, `from_state_store`) onto the trait or duplicate | S |
| Conformance test parity — the existing `worker/tests/pg_*.rs` suite (pg_conformance, pg_seen, pg_gc, pg_snapshot, pg_recording, pg_metadata) re-run against Oracle | M — needs an Oracle test instance in CI |
| `spawn_blocking` wrapping if using the (blocking) `oracle` crate | M — pervasive, touches every call |

**Total: a multi-day / ~1–2 week effort**, dominated by (a) the ~50-method Oracle
impl with dialect rewrites and (b) the async/blocking impedance mismatch. It is **not**
a "flip a feature flag" job, because sqlx cannot reach Oracle at all.

---

## 4. Recommendation

Before building any of this, decide **why Oracle**:

- **If it's "Oracle Cloud gives us a free/managed DB and we assumed that means Oracle
  DB":** Oracle Cloud (OCI) offers **managed PostgreSQL** and you can run Postgres on
  the OCI compute VM directly. Staying on Postgres = **zero** of section 2 — the native
  worker already targets it. This is almost certainly the right call: the VM is the
  deploy target, the DB engine is a free choice, and Postgres is what's built + tested.
- **If Oracle Autonomous DB is a hard requirement** (org mandate, existing licence,
  data-residency): then the section-2/3 work is real and worth a proper spike —
  start with a throwaway `OracleStateStore` implementing just `is_seen`/`mark_seen`
  + one MERGE upsert against a real Autonomous instance, to shake out the driver,
  Instant Client, JSON, and `spawn_blocking` story before committing to all ~50 methods.

**My steer:** keep Postgres on the Oracle Cloud VM (or OCI managed Postgres) unless
there's a mandate for Oracle DB specifically. The abstraction is ready if we ever need
the swap, but the driver reality makes it a genuine port, not a config toggle.

---

## Appendix — coupling map (file:line)

- Trait: `core/src/state.rs:560` (`StateStore`, ~50 methods, RPITIT → not object-safe)
- 2nd impl proof: `MemStateStore` in `core/src/state.rs` test module
- Generic consumers: `core/src/pause_gate.rs:52`, `core/src/retry_gate.rs:131`,
  `core/src/dispatch.rs:9`, `trade-control-cron/*`
- PG impl: `worker/src/pg.rs` (65KB, 42 runtime queries, 161 binds, 11 ON CONFLICT,
  31 jsonb refs, 0 compile-time macros, 0 transactions)
- PG metadata: `worker/src/pg_accounts.rs`
- PG recording (no trait): `worker/src/recording_pg.rs`
- Concrete injection sites: `worker/src/main.rs:59`, `worker/src/http.rs:74-75,343`,
  `worker/src/scheduler.rs`, `worker/src/native_cron.rs`
- Migrations (all PG dialect): `worker/migrations/0001_state.sql` …
  `0004_order_bodies.sql`
- Driver: `sqlx` 0.9 `postgres` feature (`worker/Cargo.toml`) — no Oracle support
