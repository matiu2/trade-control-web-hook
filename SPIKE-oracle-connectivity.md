# Spike plan: Oracle DB connectivity (run the moment creds land)

**Companion to** `SCOPING-oracle-db-swappable.md`. That doc says *what* the full port
costs. This one says *what to do first* — a throwaway, ~half-day connectivity spike that
de-risks the whole port before we commit to implementing all 56 `StateStore` methods.

**Context:** another Claude is standing up an **Oracle DB** instance (Autonomous /
DBCS) for testing. Creds/DSN are pending. `sqlx` **cannot** talk to Oracle, so the
native worker's Postgres store does not transfer — this spike proves out the *new*
driver stack in isolation.

**Golden rule:** the spike is **throwaway**. Do it on a scratch branch, prove the four
unknowns, write down what you learned, then throw the code away and implement properly.
Do **not** try to grow the spike into the real `OracleStateStore`.

---

## What the spike must prove (the 4 unknowns, in order)

The whole point is to hit each Postgres→Oracle friction point *once* against a real
instance, so the full port has no surprises. In priority order:

1. **Driver + connect.** Can we open a pooled connection from Rust to this Oracle
   instance at all? This is the biggest unknown — it drags in the native client.
   - Add the `oracle` crate (ODPI-C bindings). It needs **Oracle Instant Client**
     `.so` libraries present at *both* build and runtime. On the VM: install Instant
     Client, set `LD_LIBRARY_PATH` (or `ORACLE_HOME`). Autonomous DB additionally needs
     a **wallet** (`tnsnames.ora` + `cwallet.sso`) — set `TNS_ADMIN` to the wallet dir;
     the DSN is then the wallet alias (e.g. `mydb_high`), *not* a `host:port/service`.
   - **The `oracle` crate is blocking**, not async. Confirm the `spawn_blocking`
     wrapper pattern works and measure a round-trip latency. (If we discover an async
     Oracle driver worth trusting, note it — but assume blocking.)

2. **One `MERGE` upsert.** Pick the simplest upsert from `pg.rs` — the `seen` table
   (`is_seen`/`mark_seen`, single `text` PK, `timestamptz`, no jsonb). Rewrite its
   `INSERT ... ON CONFLICT (id) DO UPDATE` as an Oracle `MERGE INTO seen USING dual ...
   WHEN MATCHED ... WHEN NOT MATCHED ...`. Prove the round-trip: mark seen, read it
   back, confirm the TTL predicate (`expires_at > SYS_EXTRACT_UTC(SYSTIMESTAMP)`) works.
   This validates: positional binds (`:1` vs `$1`), timestamptz mapping, and the MERGE
   rewrite that all 11 upserts will need.

3. **One `jsonb` round-trip.** Pick a jsonb-bearing table — `trade_plan` (`body jsonb`,
   no TTL, simple key). Store a `serde_json::Value` and read it back byte-identical.
   Decide the Oracle column type here: native `JSON` (21c+, cleanest) vs `CLOB` +
   `IS JSON` (works everywhere). **This decision drives all ~15 jsonb columns** — get it
   right once. Confirm serde parity: the worker relies on jsonb being the *exact* serde
   shape KV/Postgres stored (a serialisation identity, per the migration comments).

4. **`IS NOT DISTINCT FROM` on a nullable key.** Pick one nullable-`account` table —
   `cooldown` (`COALESCE(account,'')` unique index + `IS NOT DISTINCT FROM` lookups).
   Prove the Oracle rewrite of NULL-safe equality (`DECODE(account, :1, ...)` or
   explicit `(account = :1 OR (account IS NULL AND :1 IS NULL))`) and the function-based
   unique index (`NVL(account,'')`). This validates the pattern shared by ~10 tables.

If all four pass, the remaining ~50 methods are **mechanical** — same four patterns,
repeated. If any *fails* or is uglier than expected, we learn it now, on ~4 tables, not
after porting 56 methods.

---

## Mechanics

- **Branch:** `spike/oracle-connectivity` off `main`. Worktree as a sibling
  (`../tcwh-oracle-spike`) per the parent CLAUDE.md worktree rule.
- **Where the code goes:** a single throwaway `worker/src/bin/oracle_spike.rs` (a
  `#[tokio::main]` that does the 4 probes and prints results) + a scratch
  `oracle_spike.sql` with the 4 Oracle-dialect table stubs. Do **not** touch
  `worker/src/pg.rs`, the trait, or the migrations.
- **Creds:** read the DSN/wallet path from env (`ORACLE_DSN`, `TNS_ADMIN`,
  `ORACLE_USER`, `ORACLE_PASSWORD`) — never hardcode, never commit the wallet.
  Add the wallet dir + any `*.sso`/`tnsnames.ora` to `.gitignore` first.
- **No unwrap/expect** outside the spike's own `main` (color_eyre `?`), tracing for
  logs — same house rules, even for throwaway code.

## Deliverable

A short `SPIKE-oracle-findings.md` answering, for each of the 4 unknowns: *did it work,
what was the exact Oracle syntax, any gotchas.* Plus the go/no-go call on the driver
(blocking `oracle` crate acceptable? latency ok?) and the **jsonb column-type decision**
(native `JSON` vs `CLOB`). That doc feeds directly into the real port.

## Then — the real port (only after the spike is green)

Per `SCOPING-oracle-db-swappable.md` §3, in dependency order:
1. `OracleStateStore` — 56 methods, applying the 4 proven patterns.
2. `OracleMetadataStore` (4 methods) + `recording_oracle.rs` (4 free fns, re-route the
   `.pool()` leaks in `native_cron.rs` / `http.rs:343`).
3. Oracle-dialect migrations (4 files), run via the `oracle` crate (not sqlx `migrate`).
4. Runtime backend selection — `enum AnyStore { Pg(..), Oracle(..) }` impl'ing
   `StateStore` (trait is RPITIT → not object-safe, so enum-dispatch, not `dyn`) +
   a `backend = "postgres" | "oracle"` field on `config.database`.
5. Re-run `worker/tests/pg_*.rs` conformance suite against Oracle (needs the test
   instance in the loop).

**Keep Postgres working throughout** — this is *swappable*, additive, not a replacement.
The staging/demo path stays on Postgres until Oracle passes the same conformance suite.
