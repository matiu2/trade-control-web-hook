# Migration plan — Cloudflare Worker → native VM + Postgres

**Goal:** retire the Cloudflare Workers/KV/R2 deployment and run the trade-control
engine as a single long-lived native Rust binary on a VM (Oracle Always Free →
Pi → VPS), backed by Postgres, with the option to add a broker websocket tick
stream for sub-second intrabar reaction.

**Why:** CF was chosen when this was *only* a TradingView-webhook relay. It has
since grown a stateful cron engine, plan lifecycle, break-even management, and
blackout logic. That is a long-lived stateful service, not an edge function. CF
fights us on three axes now: per-operation KV billing, no persistent outbound
websockets (so no tick stream), and the awkward index-blob hack KV forces.

---

## What's already portable (the good news)

The hard architectural work is **already done**. The codebase is layered so the
business logic never touches Cloudflare:

- **`core/`** — pure logic: intents, gates, rules, retry gate, breakeven,
  pause gate, signals (Pine port), `StateStore` *trait*. Compiles native today.
- **`engine/`** — `evaluate_plan`, simulator. Pure. Compiles native today.
- **`StateStore` is already a trait** (`core/src/state.rs`, 53 async methods).
  The module doc literally says: *"A trait keeps the dispatch logic
  transport-agnostic so a non-CF deployment (e.g. self-hosted on a home machine)
  can swap in a file-backed store later."* We are now cashing that in.
- **`MemStateStore`** already exists (for replay) — proof a second backend works
  end-to-end. `PgStateStore` is the same shape against Postgres.
- **`Broker` is a trait** (`core/src/broker.rs:246`) — TradeNation/OANDA REST.
  Already poll-based (`get_quote`, `get_candles`, `place_entry`, `amend_stop`).

### What is Cloudflare-coupled (the work to replace)

Everything that imports `worker::` (≈30 files). These fall into a few buckets:

| CF thing | Where | Native replacement |
|---|---|---|
| `#[event(fetch)]` HTTP entry | `src/lib.rs:107` | **axum** router |
| `#[event(scheduled)]` cron | `src/cron.rs:45` | **tokio interval** scheduler |
| `KvStateStore` | `src/state/kv.rs` (2109 lines) | **`PgStateStore`** (new) |
| R2 recording / tick bundles | `src/recording.rs`, `src/tick_recording.rs`, `src/r2_purge.rs` | Postgres table or local files |
| `env.secret()` / `env.var()` | throughout `src/` | **env vars / config file** (`figment`/`envy`) |
| `env.kv(NAMESPACE)` wiring | `src/lib.rs:192` | construct `PgStateStore` once at startup |
| `tracing_console` (CF logs) | `src/tracing_console.rs` | **tracing_subscriber** (already the house style) |
| `worker::Response` in dispatch | `src/lib.rs` | axum `IntoResponse` |

The **cron submodules** (`src/cron/*.rs`) are mostly portable logic with a thin
`&Env` argument for state/broker access. They become plain async fns taking
`&PgStateStore` + `&dyn Broker`. The scheduled handler body (an ordered list of
upkeep jobs) maps 1:1 onto a tokio scheduler tick.

---

## Target architecture

```
                ┌──────────────────────────────────────────────┐
   TradingView  │            native binary (one process)       │
   webhook ───► │  axum receiver  ──┐                          │
                │                   ├─► engine::evaluate_plan ──┼─► Broker REST
   (optional)   │  tokio scheduler ─┤   (core/engine UNCHANGED) │   (TN/OANDA)
   TN websocket │  (15-min sweep)   │                          │   place/close/
   tick stream ─┤                   │                          │   amend (SL+TP
   (phase 2) ──►│  ws tick task ────┘                          │   held broker-side)
                │                   │                          │
                │                   ▼                          │
                │              PgStateStore ──────────────────►│ Postgres
                └──────────────────────────────────────────────┘
```

One process. Three input sources (HTTP, timer, optional websocket) all funnel
into the same `evaluate_plan`. State in Postgres. Broker via the existing trait.

---

## Crate layout

New binary crate `worker-native/` (workspace member). Keeps the existing CF
crate compiling in parallel during the cutover, so we never have a broken tree.

```
worker-native/
  Cargo.toml          # axum, tokio (full), sqlx (postgres,chrono), figment/envy,
                      # tracing-subscriber, tracing-error, color-eyre
  src/
    main.rs           # config → connect pg → build store+broker → spawn tasks
    config.rs         # env/file config (secrets, account store path, pg url)
    http.rs           # axum router: POST /webhook, GET /status, admin routes
    scheduler.rs      # tokio interval; calls the upkeep jobs in order
    dispatch.rs       # the run_action/run_enter logic, lifted off worker::Response
  ...
core/
  src/state/pg.rs     # NEW: PgStateStore — impl StateStore for Postgres,
                      # directly in core. sqlx is a normal dependency now.
```

**Decided:** wasm purity is no longer a requirement (the CF crate is being
retired). `PgStateStore` lives **directly in `core`** with `sqlx` as a plain
dependency — no `postgres` feature flag, no separate crate, no "must not leak
into the wasm build" gymnastics. Once Phase 3 removes the CF crate, `core` drops
its `cdylib`/wasm target and is just a normal native lib.

---

## Postgres schema

Postgres *removes* the index-blob hack. KV can't list, so `kv.rs` maintains
`index:seen` / `index:vetos` JSON blobs by read-modify-write (racy, billed). In
Postgres, listing is `SELECT … WHERE expires_at > now()`. Delete the indexes.

One table per state family, every TTL'd row carries `expires_at timestamptz`;
a background sweep (or partial index + lazy filter) prunes. Sketch:

```sql
-- replay protection (was seen:<id>)
CREATE TABLE seen (
  id          text PRIMARY KEY,
  action      text NOT NULL,
  seen_at     timestamptz NOT NULL,
  outcome     text NOT NULL DEFAULT '',
  trade_id    text,
  expires_at  timestamptz NOT NULL
);
CREATE INDEX seen_expiry ON seen (expires_at);

-- cooldowns / preps / vetos / pauses / news-windows: same pattern,
-- composite keys mirroring the KV key structure
--   cooldown(account, instrument)            expires_at
--   prep(account, instrument, step)          expires_at
--   veto(account, trade_id, instrument, name) expires_at
--   ...

-- plans + plan-state (per-trade, NO ttl — matches Bug #15 fix)
CREATE TABLE trade_plan  (id text PRIMARY KEY, body jsonb NOT NULL, ...);
CREATE TABLE plan_state  (plan_id text PRIMARY KEY, body jsonb NOT NULL, ...);
CREATE TABLE archived_plan (...);            -- plan show scans this (v50)

-- entry attempts (retry gate), order bodies, control-event audit trail,
-- mw_state, spread/blackout records, blackout windows
```

Map the `StateStore` semantics exactly — the per-trade-row vs control-row TTL
split from Bug #15 must be preserved (per-trade rows: no TTL; control rows:
window TTL). `jsonb` for the structured bodies (`TradePlan`, `PlanState`) keeps
serde round-trips trivial and lets us query inside them later if needed.

---

## Phased delivery (each phase ships green, no intermediate CF change)

**Phase 0 — scaffold (no behaviour change).**
- Add `worker-native/` + `trade-control-pg/` workspace members.
- `PgStateStore` skeleton: implement `StateStore` against Postgres, port the
  schema, write the same trait tests the KV store passes (reuse the
  MemStateStore test suite as a conformance harness across all three backends).
- Gate: `PgStateStore` passes the same conformance tests as `KvStateStore`.

**Phase 1 — native HTTP + scheduler, poll-based (CF feature parity).**
- axum receiver reproducing the dispatch path (lift `run_action` off
  `worker::Response`; return `IntoResponse`). Reuse signature verification,
  intent parsing, gates — all in `core`, unchanged.
- tokio scheduler running the same ordered upkeep list as `src/cron.rs:45`
  (session refresh → sweep → blackout watch → breakeven watch → engine tick →
  NY-close-edge → daily blackout-hours), each ported cron submodule taking
  `&PgStateStore` + `&dyn Broker` instead of `&Env`.
- Config: secrets from env/file; account store stays at
  `~/.config/tradenation/accounts.enc` (already native — the WASM-reuse memo
  flagged the Sheets/account stack as non-WASM, which is *why* it never moved to
  CF cleanly; native is its natural home).
- Recording: R2 → a Postgres `recordings` table or local JSONL files
  (`tick_bundle` / `req` prefixes become tables/dirs).
- Gate: run the **existing replay/tick-bundle harness** against the native
  binary and diff decisions vs the CF worker on the same inputs. Decision parity
  is the acceptance test (the `strategy_changes_in_both` discipline pays off —
  logic is shared in core/engine, so parity should be exact).

**Phase 2 — websocket tick stream (the capability CF can't give us).**
- Add a `Broker::subscribe_ticks(instrument) -> Stream<Tick>` method (TN
  websocket; OANDA pricing stream). A tokio task feeds ticks into the engine.
- **Only wire intrabar-legitimate levels to tick reaction**: `too-low`
  (pcl-exhausted, fires on any straddle by design), SL-bounded invalidations.
  **Keep close-confirmed logic close-confirmed** — `too-high` is close-confirm
  now (worker v-current), and the Pine-parity detector confirms on closed bars.
  Do NOT let tick reaction silently convert close-confirm into wick-trigger.
- The 15-min scheduler stays as the backstop/heartbeat; ticks just let us react
  *between* ticks for the levels where that's correct.

**Phase 3 — cut over + retire CF.**
- Deploy native binary to the chosen host. Run both in parallel for one window
  (native in dry-run/shadow, CF live) and diff. Promote native to live.
- Decommission the CF workers (dev/staging) + KV namespaces + R2 buckets.
- `wrangler.toml`, deploy-*.sh, the branch-as-environment model all retire;
  replace with a much simpler env-per-host config (a `config.{dev,staging,prod}.toml`
  + a systemd unit per environment).

---

## Host & ops (native binary)

- **Process supervision:** `systemd` unit with `Restart=always`. On crash/reboot
  it comes back; on startup it reloads live plans from Postgres and re-arms the
  scheduler. (Plans are durable in PG — no in-memory state to lose.)
- **Postgres:** **two Oracle Arm instances** — one compute, one dedicated PG.
  So PG is **remote over the network**, not a local unix socket. Implications:
  - Keep both instances in the **same region / availability domain / VCN** so the
    hop is sub-ms on Oracle's **private subnet**, not the public internet.
  - Compute connects to PG over the **private VCN IP**, never the public IP. PG
    must not listen on a public interface — its security list/firewall allows
    `5432` only from the compute instance's private subnet. (Your state store
    references broker accounts; it should never be internet-reachable.)
  - Use a warm **`sqlx::PgPool`** (pooled connections) with reconnect + backoff;
    don't open a connection per state op.
  - Data is tiny, so this is cheap — but it adds a failure mode (see below).
    Back up with `pg_dump` (cron → Oracle Object Storage or the compute disk).
- **Blackout/power resilience (your point, and it's correct):** every order is
  placed *with SL+TP*, which the **broker** holds. A dead host therefore cannot
  fail to *exit* an open position — worst case it misses a *new entry* or a
  *strategy-side* action (break-even amend, reversal-close) during downtime.
  So the host needs to be reliable enough not to miss entries, but a blackout is
  not catastrophic. This is what makes the Pi viable. (Mitigation for the
  strategy-side gap: on restart, the breakeven_watch / reversal sweep re-runs and
  catches up any amend it missed — verify this catch-up path explicitly.)
- **Two-instance failure mode (new with split compute/PG):** now *both* boxes
  plus the private link must be up. If **PG** is down, compute can't read plans
  or write state. Plans are durable in PG, so compute recovers cleanly once PG
  returns — no lost state — but the engine stalls meanwhile. Mitigations:
  (a) `PgPool` reconnect with backoff so a brief blip self-heals;
  (b) optionally cache the active plan set in compute memory so a short PG
  outage doesn't immediately stall evaluation (re-sync on reconnect);
  (c) the broker still holds SL/TP throughout, so an open position is safe even
  if *both* boxes die — the residual exposure is the same missed-entry /
  missed-amend window as the single-box case. Net: one more failure mode, no new
  catastrophic loss. Keep PG and compute in the same AD so the link itself is
  Oracle's internal network, not the public internet.
- **Environments:** instead of git-branch-as-env, run N systemd services with
  different config files + different Postgres databases (or schemas). Same
  binary, different config. Much simpler than three `wrangler.toml`s.

### Host options (you're investigating Oracle)
- **Oracle Cloud Always Free** — genuinely free forever (Arm Ampere, up to
  4 cores / 24 GB). Holds websockets. Arm target = same as the eventual Pi.
  **Best match for a persistent process.** ← you're checking this.
- **Raspberry Pi (home)** — your long-term real-money host; cellular failover +
  battery. Same Arm binary. Physical key custody.
- **Fly.io** — no standing free tier anymore (usage-billed, ~cents/mo for a tiny
  always-on machine). Good DX, holds websockets. Not free.
- **GCP e2-micro free** — one small always-free US VM; tighter RAM.

---

## Open decisions (need your call)

1. ~~**`PgStateStore` location**~~ → **REVISED DECISION:** a **fresh `worker`
   crate** (`trade-control-worker`, dir `worker/`), NOT in `core`. Reason: `core`
   still compiles to wasm for the in-tree CF worker until Phase 3, and sqlx is
   heavy native-only — putting it in `core` (even feature-gated) risks the wasm
   build. The new `worker` crate never targets wasm, depends on `core`, and
   implements `core::StateStore`. It also becomes the home for the Phase 1 axum
   receiver + tokio scheduler — i.e. it IS the `worker-native` crate, created now.
   **Port old-worker pieces into it as needed, not in bulk.**
2. ~~**Recording sink**~~ → **DECIDED:** Postgres table (single source of truth).
   The downstream tax-tracker (currently pulls R2 `req/`+`ticks/` via S3) gets
   taught to read PG — tracked as a follow-up, not a blocker for the engine.
3. ~~**DB driver**~~ → **DECIDED:** **sqlx** (async, compile-time-checked queries,
   built-in migrations).
4. ~~**CF coexistence**~~ → **DECIDED:** keep CF crate in-tree during cutover so
   the replay harness can diff native-vs-CF for parity; remove in Phase 3.
5. **Websocket in phase 1 or defer to phase 2** → defer (recommend) — get poll
   parity first, prove decisions match, then add ticks.

### Host: Oracle Cloud, region `uk-london-1` (DECIDED)
Co-located with TradeNation's real trading backend (`portal.cube.finsatechnology.com`
= AWS `eu-west-2`, London). OANDA is behind Cloudflare anycast (no region pull).
Two Arm instances: compute + Postgres, same region/AD, private VCN.

---

## Risks / things not to break

- **Decision parity is the whole game.** The `[[strategy_changes_in_both]]`
  discipline means logic lives in core/engine; the native shell must call the
  *same* functions. Use the replay harness as the parity gate — if native and
  CF disagree on a recorded tick, that's a porting bug.
- **TTL semantics (Bug #15):** per-trade rows no-TTL, control rows window-TTL.
  Encode in the schema/sweep, not just in code.
- **Account store is native-only already** — moving to a native host actually
  *simplifies* the account/secret path (no KV metadata shim, no secret_resolver
  indirection).
- **Replay protection + retry gate** must behave identically — both already in
  `core`, just give them the PG store.
- **Don't regress close-confirm into wick-trigger** when adding ticks (phase 2).

---

## Phase 0 status — DONE & GREEN (2026-06-29)

The Postgres `StateStore` layer is complete and proven:

- `trade-control-worker` crate (`worker/`), workspace member, edition 2024.
- `PgStateStore` — all **53** `StateStore` methods over Postgres
  (`worker/src/pg.rs`), schema `worker/migrations/0001_state.sql` (17 typed
  tables), applied to the dev DB.
- **Cross-backend conformance harness** `core::state::conformance::run_all`
  (gated `test-support`): one set of behavioural assertions, run against
  **both** `MemStateStore` (in `core`) and `PgStateStore` (in `worker`).
  **Green on both.** It caught four real parity bugs (NULL-account PK, TTL
  clamp, µs/ns timestamp precision, `ON CONFLICT` on account) — all fixed.
  See `TODO.md` for the full writeup.
- `snapshot()` (Pg-only cross-family aggregation) tested.
- Builds offline (runtime queries, no `query!` macro / no `.sqlx` cache).

The StateStore seam is the hard part of the migration and it's done. What
remains is the native *shell* around it (Phase 1).

## Phase 1 — OPEN QUESTIONS for the user (do NOT guess these)

Phase 1 (axum receiver + tokio scheduler) has design forks that need a
decision before coding — flagged here rather than guessed:

1. **HTTP server shape** — bind addr/port, TLS termination (nginx/caddy in
   front, or rustls in-process?), graceful-shutdown signal handling. Behind a
   reverse proxy on the Oracle box, or exposed directly?
2. **Scheduler cadence** — CF cron fired the upkeep list on a fixed `*/N`.
   Native can self-pace (a tokio interval). Keep the same N-minute tick, or
   tighten now that we're not paying per-invocation? (Phase 2 websocket changes
   this calculus again.)
3. **Recording sink schema** — DECIDED it's a Postgres table, but the table
   shape (one `recordings` table with a `kind` + jsonb, vs separate `req` /
   `tick_bundle` tables) and whether the tax-tracker's S3 pull is replaced now
   or bridged later.
4. **`&dyn Broker` construction** — the scheduler needs a broker per account
   from `~/.config/tradenation/accounts.enc`. Confirm the native binary owns
   that store directly (it should — it's native already) and how secrets
   (`OANDA_API_KEY` etc.) are sourced (env, file, or the same enc store).
5. **Config file format** — env vars (12-factor) vs a TOML config the binary
   reads. The CF model was all secrets; native can have a real config file.

Recommendation when you're ready: do Phase 1 as its own branch off this one,
porting `src/cron.rs`'s submodules one at a time, each taking `&PgStateStore`
+ `&dyn Broker`, with the replay harness as the decision-parity gate.
