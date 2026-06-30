# trade-control-worker — VM deployment runbook

The operational companion to `MIGRATION-VM-POSTGRES.md` (which holds the *why*:
host = Oracle Cloud `uk-london-1`, co-located with TradeNation's `eu-west-2`
backend; two Arm instances, compute + Postgres on a private VCN). This file is
the **step-by-step how**: build → provision → configure → run → verify. It
targets the **native binary** (`worker/`, package `trade-control-worker`), *not*
the deprecated Cloudflare wasm worker.

> **Status:** Phase 1 (native shell) is code-complete on branch
> `feat/native-runtime`. This runbook is the Phase-2 procedure to stand it up on
> the VM against a **demo** account first. Nothing here has been merged to `main`
> or run on the real box yet — treat every command as "to be exercised on first
> deploy", and correct this file from what actually happens.

---

## 0. What you're deploying

| Piece | What it is |
|---|---|
| `trade-control-worker` | the native Rust binary: axum HTTP receiver (`POST /`) + in-process tokio cron scheduler, backed by `PgStateStore`. |
| Postgres | the state store (replaces Cloudflare KV) + the recording sink (`request_records`, `tick_bundles`) + the account index (`accounts`). |
| reverse proxy (caddy/nginx) | terminates TLS, forwards to the worker on `127.0.0.1`. The worker handles **no certs** (Phase-1 decision #1). |
| TradeNation enc account store | `~/.config/tradenation/accounts.enc` — TN logins resolve by name from here, not from env. Copy it to the box. |

The worker binds **plain HTTP on loopback** and is only reachable through the
proxy. TradingView (or `tv-arm`) POSTs the signed alert to the public HTTPS URL;
the proxy forwards it.

---

## 1. Build the binary (cross-compile for Arm)

The VM is **aarch64** (Oracle Ampere). Build a release binary for
`aarch64-unknown-linux-gnu`. From the repo root (this is a Cargo **workspace** —
build by package):

```sh
# one-time: add the target + a linker
rustup target add aarch64-unknown-linux-gnu
# Debian/Ubuntu cross linker (or use `cross`):
sudo apt-get install -y gcc-aarch64-linux-gnu

# build just the worker (and its workspace deps) for Arm, release
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo build -p trade-control-worker --release \
  --target aarch64-unknown-linux-gnu
# → target/aarch64-unknown-linux-gnu/release/trade-control-worker
```

Alternatives, in order of preference if the above is fiddly:
- **`cargo install cross`** then `cross build -p trade-control-worker --release
  --target aarch64-unknown-linux-gnu` (Docker-backed, no host linker setup).
- **Build on the VM itself** — the Ampere instance has 4 cores / 24 GB; a native
  `cargo build -p trade-control-worker --release` on the box avoids
  cross-compilation entirely. Slower first build, simplest toolchain.

Copy the binary up:

```sh
scp target/aarch64-unknown-linux-gnu/release/trade-control-worker \
  ubuntu@<vm>:/opt/trade-control/bin/trade-control-worker
```

> sqlx note: the worker uses **runtime** queries (no `query!` macro / no `.sqlx`
> cache), so the build needs **no live database** — it compiles offline. Good for
> CI and cross builds.

---

## 2. Provision Postgres

On the DB instance (or the same box for the first demo run):

```sh
sudo apt-get install -y postgresql
sudo -u postgres createuser --pwprompt trade_control
sudo -u postgres createdb -O trade_control trade_control
# connection string for the config below:
#   postgresql://trade_control:<pass>@<db-host>:5432/trade_control
```

Migrations are applied by the worker itself on boot (`PgStateStore::migrate()` —
runs `worker/migrations/000{1..4}_*.sql`): state tables, accounts, recordings,
order bodies. **No separate migrate step** — just start the worker once and check
the log line `Postgres connected + migrated`.

Keep the DB on the **private subnet** — the worker reaches it over the VCN, never
the public internet. Back it up with `pg_dump` on a cron (→ Object Storage).

---

## 3. Configure: `trade-control.toml` (non-secret)

Lives on the box, e.g. `/opt/trade-control/trade-control.toml`. Only
`database.url` is required; everything else has the defaults shown.

```toml
[http]
bind_addr = "127.0.0.1"   # loopback — proxy-only reachable (default)
port      = 8787          # default

[database]
url             = "postgresql://trade_control:<pass>@<db-host>:5432/trade_control"
max_connections = 10      # default

[scheduler]                # per-task tokio intervals, seconds (Phase-1 #2)
engine_secs       = 60     # engine tick: re-eval every plan vs fresh candles
upkeep_secs       = 900    # session refresh + order sweep + breakeven/spread watch
daily_tick_secs   = 900    # wakes often, self-gates on the hour (NY-close blackout, market-hours)
expiry_sweep_secs = 3600   # DELETE … WHERE expires_at < now() — native TTL stand-in
```

> The DB password sits in this file. If you need it out of the file, point
> `database.url` at a value your process manager interpolates from a secret, or
> keep the file `chmod 600` + owned by the service user. For the demo box,
> `chmod 600` is fine.

---

## 4. Secrets: environment variables (never in the TOML)

Loaded by `Secrets::from_env()` at boot. Put them in a `chmod 600` env file the
systemd unit reads (`EnvironmentFile=`), e.g. `/opt/trade-control/worker.env`:

```sh
# --- required ---
SIGNING_KEY=<hex>          # HMAC key verifying signed intent bodies; also the X-Diag-Key.
                           # MUST match the key tv-arm / trade-control sign with (hex).
ADMIN_KEY=<secret>         # auth for the (not-yet-ported) /admin/* write routes.

# --- optional, with defaults ---
MAX_RISK_PCT_PER_TRADE=1.0 # worker-wide risk cap %  (default 1.0)
MAX_OPEN_POSITIONS=3       # worker-wide max open positions (default 3)

# --- OANDA (only if an OANDA account is configured) ---
OANDA_API_KEY=<token>      # OANDA bearer token, shared across sub-accounts
OANDA_LIVE=false           # global practice/live flag; named accounts override via their `kind`.
                           # KEEP false for the demo box.

# --- per-instrument pip override (optional, open-ended; read lazily) ---
# PIP_SIZE_USD_JPY=0.01     # only if an intent doesn't bake its own pip_size
```

Notes:
- A **TradeNation-only** demo can omit `OANDA_API_KEY` / `OANDA_LIVE` entirely.
- A numeric var that is *present but unparseable* is a **hard boot error** (a typo
  in a risk cap must not silently fall back to the looser default) — so leave a
  var unset to take its default rather than setting it to a bad value.
- **Do not** export `TRADE_CONTROL_ENDPOINT` here or globally — that's a *CLI*
  override and has nothing to do with the worker; setting it globally would
  silently re-point the suffixed CLIs (see CLAUDE.md).

### Account credentials are NOT env vars

Per-account creds resolve out-of-band:
- **TradeNation** logins → the enc store `~/.config/tradenation/accounts.enc`
  (resolved by the account *name* on each intent). Copy this file to the service
  user's home on the box.
- **OANDA** sub-account ids → the Postgres `accounts` table (seed it with the
  account-management path; the OANDA *token* is the shared `OANDA_API_KEY` env).

So before the first real intent, ensure the named account on the intent exists in
both: the enc store (TN) / the `accounts` table (OANDA metadata: name, broker,
kind=demo, caps).

---

## 5. systemd unit

`/etc/systemd/system/trade-control-worker.service`:

```ini
[Unit]
Description=trade-control native worker (axum + tokio cron, Postgres-backed)
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
Type=simple
User=trade-control
WorkingDirectory=/opt/trade-control
EnvironmentFile=/opt/trade-control/worker.env
# RUST_LOG controls the tracing env-filter; bump to debug while bedding in.
Environment=RUST_LOG=info
ExecStart=/opt/trade-control/bin/trade-control-worker /opt/trade-control/trade-control.toml
Restart=on-failure
RestartSec=5
# graceful shutdown: the binary traps SIGTERM (systemd stop) + SIGINT and
# drains axum before exiting, so the default KillSignal=SIGTERM is correct.
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now trade-control-worker
journalctl -u trade-control-worker -f
```

Expected boot log lines (in order):
1. `loaded config from … (bind 127.0.0.1:8787)`
2. `Postgres connected + migrated`
3. the scheduler + axum serve starting.

If boot fails: a missing required env var → `missing required env var SIGNING_KEY`;
a bad `database.url` → `connecting to Postgres: …`; a non-hex signing key →
`SIGNING_KEY is not valid hex`. All are loud, fail-fast.

---

## 6. Reverse proxy (TLS)

The worker speaks plain HTTP on `127.0.0.1:8787`. Front it with **caddy** (gets
Let's Encrypt + renews automatically — least config):

`/etc/caddy/Caddyfile`:
```
hook.<your-domain> {
    reverse_proxy 127.0.0.1:8787
}
```
```sh
sudo systemctl reload caddy
```

That's the whole TLS story — caddy provisions the cert on first request. (nginx +
certbot works too if you prefer; the worker side is identical.)

> **Health check:** the worker exposes **`GET /health`** — a cheap `200 OK`
> liveness probe (no DB round-trip, so it stays green through a transient
> Postgres blip rather than flapping the service out of the proxy). Point your
> uptime/proxy check at it:
> ```sh
> curl -s -o /dev/null -w '%{http_code}\n' https://hook.<domain>/health   # → 200
> ```
> The wasm worker's richer `/diag/*` and `/admin/*` routes are still **not
> ported** (see `worker/src/http.rs` router doc) — that's the Phase-2 admin
> surface, separate from this liveness probe.

---

## 7. Smoke test (demo account, dry-run first)

Order matters — prove each layer before the next.

1. **Process up:** `systemctl is-active trade-control-worker` → `active`; the
   three boot log lines present.
2. **Local reachability (bypass proxy):** from the box,
   ```sh
   curl -s -o /dev/null -w '%{http_code}\n' localhost:8787/health   # → 200
   ```
   A `200` confirms axum is serving. A 000/connection-refused means the worker
   isn't bound. (A `POST localhost:8787/ -d garbage` → **4xx** additionally
   confirms the dispatcher thread + sig-verify path.)
3. **Through the proxy (TLS):** `curl https://hook.<domain>/health` → `200` over
   HTTPS — confirms caddy → loopback forwarding + the cert.
4. **A real signed control intent (no broker, no risk):** sign a `status` or
   `prep` with `trade-control-<env>` (whose baked webhook = the public URL) and
   POST it. Expect a 200 and a `request_records` row in Postgres. This exercises
   sig-verify + dispatch + recording end-to-end with zero broker exposure.
5. **A dry-run enter on the demo account:** arm a setup with `--broker-dry-run`
   (or `dry_run: true` on the enter intent). The worker runs the **full** gate
   chain (resolve, retry, cooldown, prep, veto, spread-blackout, SL-spread floor)
   and logs the placement **without** POSTing to the broker. Confirm in the log
   the entry was accepted + "dry-run" and the engine tick keeps the plan alive.
6. **A live demo enter:** drop `--broker-dry-run`, keep the account `kind=demo`
   (so `OANDA_LIVE`/the account routes to practice). One real demo placement.
   Reconcile the broker fill against the worker log + the `request_records` /
   `tick_bundles` rows.

Only after 1–6 are clean is the box a candidate to take over a week's demo
trading from the staging Cloudflare worker.

---

## 8. Parity gate before trusting it

The whole migration rests on **decision parity** (`MIGRATION-VM-POSTGRES.md`
risks). The native worker, the wasm worker, and the replay all call the *same*
`core::dispatch` / `engine` / `trade-control-cron` code — parity is by
construction. The remaining differing surface is the **broker**, characterised in
`PARITY.md` (verdict: SOUND; the one residual is the replay's sub-bar spread
spike, now bounded after the `get_quote` fix). Use the replay harness as the
gate: if the native worker and a recorded tick disagree, that's a porting bug,
not a strategy change.

---

## 9. Known Phase-2 follow-ups (not blockers for a demo stand-up)

Tracked so they're not mistaken for bugs on the box:

- **`GET /health` exists** (liveness, §6); the richer `/diag/*` + `/admin/*`
  admin surface from the wasm worker is **not** ported yet.
- **Unnamed-account broker intent → 400.** The wasm worker had a global-OANDA
  default; the native receiver requires a *named* account for any broker action
  (`AccountResolveError::Required`). Default-account routing is a deferred TODO
  (see `native_cron.rs::resolve_meta`).
- **Per-request log capture is empty natively** — `RequestRecord.logs` isn't
  populated yet (the wasm worker buffered `console_log!` lines). The Cloudflare
  Real-time-Logs equivalent is `journalctl` for now.
- **`PlanPurge` / `PurgeOlderThan` / `MarketInfo` actions → 501** on the native
  receiver until their native glue lands.

---

## Quick reference — files & names

| Thing | Value |
|---|---|
| package / binary | `trade-control-worker` |
| config path arg | first positional arg, else `./trade-control.toml` |
| required env | `SIGNING_KEY`, `ADMIN_KEY` |
| optional env | `MAX_RISK_PCT_PER_TRADE`, `MAX_OPEN_POSITIONS`, `OANDA_API_KEY`, `OANDA_LIVE`, `PIP_SIZE_<INSTR>` |
| migrations | `worker/migrations/000{1..4}_*.sql`, auto-applied on boot |
| bind | `127.0.0.1:8787` (loopback, proxy-only) |
| routes | `POST /` (webhook) + `GET /health` (liveness) |
| TN creds | `~/.config/tradenation/accounts.enc` (by name) |
| OANDA creds | token = `OANDA_API_KEY` env; sub-account = Postgres `accounts` |
| branch | `feat/native-runtime` (unmerged) |
