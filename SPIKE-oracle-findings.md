# Oracle connectivity spike — findings

**Ran:** 2026-07-06 against the live **Oracle 19c** Autonomous DB (`uk-london-1`,
provisioned by the other instance, see `infra/oracle-db/ORACLE-DB-HANDOFF.md`).
**Verdict: GO.** All 4 unknowns from `SPIKE-oracle-connectivity.md` pass against the
real instance. The Postgres→Oracle port is de-risked; no surprises found; the SQL
rewrites are proven. Backend swap will be **compile-time (Cargo feature)** per the
operator's call — so no `AnyStore` enum / object-safety work is needed.

---

## Unknown #1 — driver + connect: **PASS** (with one gotcha)

- **Rust `oracle` crate 0.6.3** (pulls `odpic-sys` = ODPI-C, compiled via `cc`) connects
  to the live DB. Confirmed round-trip: `SELECT SYS_EXTRACT_UTC(SYSTIMESTAMP)` returns
  clean UTC.
- **Needs Oracle Instant Client** (no thin mode for the Rust crate). Used **Basic Light
  23.4** unpacked to `~/opt/oracle/instantclient_23_4`, pointed at via
  `LD_LIBRARY_PATH`. Basic Light (58 MB) is enough — omits `libociei` (only needed for
  some charsets), everything worked.
- **The crate is BLOCKING** (synchronous `oracle::Connection`). The real worker must wrap
  every store call in `tokio::task::spawn_blocking` (the async/blocking impedance the
  scoping doc flagged — confirmed real, not avoidable).
- **⚠️ GOTCHA — `sqlnet.ora` wallet path.** The wallet zip ships
  `WALLET_LOCATION = (... DIRECTORY="?/network/admin")`. The `?` means `$ORACLE_HOME`,
  which Instant Client resolves wrong → **ORA-28759: failure to open file** on connect.
  **Fix:** rewrite `sqlnet.ora`'s `DIRECTORY` to the actual unzipped wallet dir. Thin-mode
  Python (`oracledb`) ignores this and connects fine — so this bites *only* the thick
  Rust/ODPI-C path. **This is a deploy-runbook step**, not a code issue.
- **Latency:** ~284 ms/query from this dev box → London. That's the *network* (DB is in
  `uk-london-1`, dev box isn't), **not** the driver. On the OCI VM in-region this is
  sub-millisecond. Not a blocker; noted so no one panics at the dev-box number.

## Unknown #2 — MERGE upsert (the 11 `ON CONFLICT`): **PASS**

Oracle has no `ON CONFLICT`. The rewrite that works:

```sql
MERGE INTO seen d
  USING (SELECT :id AS id FROM dual) s ON (d.id = s.id)
  WHEN MATCHED THEN UPDATE SET action=:action, outcome=:outcome, expires_at=:exp
  WHEN NOT MATCHED THEN INSERT (id, action, outcome, expires_at)
    VALUES (:id, :action, :outcome, :exp)
```

- Verified: upsert twice → MATCHED path updates in place; single statement, no txn.
- **TTL predicate** `expires_at > SYS_EXTRACT_UTC(SYSTIMESTAMP)` correctly hides expired
  rows (replaces Postgres `expires_at > now()`). An expired row is invisible to reads,
  same semantics as the PG store.
- **Binds are named (`:id`)**, not positional `$1`. The `oracle` crate + `oracledb` both
  want named or `:1`-style; the `$1` → `:1` swap is mechanical across all queries.

## Unknown #3 — jsonb (the ~15 `jsonb` columns): **PASS → decision: CLOB + `IS JSON`**

- **Native `JSON` column type is NOT available on 19c** — `CREATE TABLE (body JSON)` →
  **ORA-00902: invalid datatype**. (Native JSON is 21c+.) So the scoping doc's fallback
  is the actual answer.
- **Decision: `body CLOB CONSTRAINT ... CHECK (body IS JSON)`** for every jsonb column.
  Verified a `serde_json` payload (nested objects, arrays, null, unicode, float) stored
  and read back **byte-identical** — key order and `\uXXXX` unicode escapes preserved.
  The worker's "jsonb == exact serde shape" identity holds.
- **Read gotcha:** `oracledb` (Python) auto-decodes an `IS JSON` CLOB to a `dict` on
  SELECT. To get the raw stored string use `DBMS_LOB.SUBSTR(body, 4000, 1)` (or read the
  LOB). The Rust `oracle` crate returns the CLOB as text — map to `serde_json::from_str`.
  Watch the 4000-byte inline limit if reading via `DBMS_LOB.SUBSTR`; large bodies need a
  full LOB read (the Rust crate handles this natively).

## Unknown #4 — `IS NOT DISTINCT FROM` (the 25 nullable-account lookups): **PASS**

Oracle has no `IS NOT DISTINCT FROM`. Two halves, both verified:

- **NULL-safe equality** in WHERE clauses:
  ```sql
  WHERE (account = :a OR (account IS NULL AND :a IS NULL)) AND instrument = :i
  ```
  Finds both the global (`account IS NULL`) and scoped rows correctly.
- **Functional unique index** replacing PG's `COALESCE(account,'')`:
  ```sql
  CREATE UNIQUE INDEX cooldown_key ON cooldown (NVL(account,'~g~'), instrument)
  ```
  Verified it enforces uniqueness — a duplicate global row is rejected with
  `IntegrityError`. (Sentinel `'~g~'` avoids colliding with a literal empty-string
  account; pick a sentinel that can't be a real account name.)

---

## What this means for the real port

Every risky dialect question is now answered against the live DB. The remaining port
(per `SCOPING-oracle-db-swappable.md` §3) is **mechanical repetition of 4 proven
patterns** — MERGE, CLOB+IS JSON, NULL-safe eq, `SYS_EXTRACT_UTC` TTL — across 56
methods, plus:

- **Compile-time feature swap** (operator's choice): `oracle` XOR `postgres` Cargo
  feature; `type ActiveStore = ...` alias resolves at build. **No `AnyStore` enum, no
  `dyn`, no object-safety workaround** — the RPITIT trait stays generic and each binary
  is one backend. Mirror `candle-cache`'s existing `postgres-storage` feature pattern.
- `spawn_blocking` wrapper around every `oracle` call (blocking crate).
- Oracle-dialect migrations (4 files) run via the `oracle` crate, not sqlx `migrate`.
- Re-run `worker/tests/pg_*.rs` conformance against Oracle.

## Deploy-runbook items surfaced by the spike

1. Install Instant Client (Basic Light is enough) on the OCI VM; set `LD_LIBRARY_PATH`.
2. Unzip the wallet; **rewrite `sqlnet.ora`'s `WALLET_LOCATION DIRECTORY`** from
   `?/network/admin` to the real wallet path (else ORA-28759). Set `TNS_ADMIN` to it.
3. Connect as `ADMIN` / DSN `tradectrl_low` (or `_medium`). Creds in the gitignored
   `infra/oracle-db/.admin-password` + wallet password file.
4. In-region (VM in `uk-london-1`) latency is sub-ms; the ~284 ms seen in the spike is
   the dev-box→London hop only.

**Spike code is throwaway** (scratchpad: Python probes + a standalone `oracle-rust-spike`
crate). Nothing committed to the workspace. The real `OracleStateStore` is a fresh,
proper implementation.
