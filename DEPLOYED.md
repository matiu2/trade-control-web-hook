# DEPLOYED — what's running where

Single source of truth for the deployed version tuple of each
environment. **Update this file whenever you deploy or promote.** When
something breaks in an environment, read this file, check out the listed
tags/commits, and reproduce against the recorded R2 requests.

The system has four independently-versioned parts that must interoperate:

| part | what | how it deploys |
|---|---|---|
| **pine** | the alert-emitting Pine script | **manual** paste into TradingView |
| **tv-arm** | local CLI that arms charts → signed alerts | `cargo install --path tv-arm` |
| **trade-control** (cli) | local CLI that builds/signs intents | `cargo install --path cli` |
| **backend** | the native worker (`worker/`, `trade-control-worker`) | `./deploy-{dev,staging}.sh` → systemd `--user` service |
| **contract** | the signed wire format both speak | bumped only on wire-format change |

The version numbers do **not** move in lockstep — only the **contract**
must stay compatible across a deploy. Bump `contract` only when the
message wire format changes.

## Branch → environment model

| branch | environment | worker service / port | deploy rule |
|---|---|---|---|
| `main` | dev | `trade-control-worker-dev` (:8787) | deploy freely |
| `staging` | staging (demo account) | `trade-control-worker-staging` (:8788) | deploy freely; must run **1 week unchanged + profitable** to promote |
| `prod` | prod (live account, later) | `trade-control-worker-prod` | only promoted-from-staging code; not stood up yet |

**Every environment carries a suffix** (`-dev` / `-staging` / `-prod`) on both
its CLIs and its worker binary/service. Each is a LOCAL native/Postgres worker
(Cloudflare fully retired): one Postgres server (`:5432`), one database + worker
process per env. `./deploy-{dev,staging}.sh` (branch-guarded) rebuild + install
the suffixed CLIs and roll the matching `trade-control-worker-<suffix>` systemd
`--user` service. There is no `wrangler`, no KV, no R2.

**Promotion (staging → prod):** if staging ran a full week with no code changes
and turned a profit, merge `staging` → `prod`, stand up the prod worker (its own
port + Postgres DB, or the Oracle DSN once OKE compute lands), deploy it, then
cut a fresh `staging` from `main` carrying the week's accumulated changes.

---

## Current state

<!-- Update the cells below on every deploy/promote. Dates are Brisbane. -->

### dev

| part | version | deployed (Brisbane) | notes |
|---|---|---|---|
| pine | `v2.5` (study title `Candle Signals v25`) | — | manual republish; sends `open` for M/W body logic. `tv-arm-dev` bakes this study title (`ENV_PINE_NAME`) — rename the chart study to match. |
| tv-arm | `v148-47-g47183b36` | 2026-10-08 09:47 | installed as `tv-arm-dev`; built from the merged broker-exit reporting revision. |
| trade-control | `v148-47-g47183b36` | 2026-10-08 09:47 | installed as `trade-control-dev` with `http://127.0.0.1:8787` baked in. Matching `tv-news`, `replay-candles`, `journal`, and shared worker operator tools rebuilt and installed. `journal-dev` displays broker-confirmed exit reasons and signed exit slippage. |
| backend | `main` @ `47183b36` | 2026-10-08 09:47 | rebuilt and installed `trade-control-worker-dev`; service active and `GET /health` returned HTTP 200. Migration 7 applied. Broker exits recorded once per account/plan/entry order; later reconcile passes skip reported exits. |
| contract | `v3` | — | unchanged by v24/v25 (`open` is optional) |

### staging

| part | version | deployed (Brisbane) | notes |
|---|---|---|---|
| pine | `v2.4` (study title `Candle Signals v24`) | — | pinned to the pre-`open` version; chart **unchanged** this deploy. v25 worker degrades gracefully when `open` is absent (rides baked geometry). `tv-arm-staging` bakes this study title. |
| tv-arm | `v148-47-g47183b36` | 2026-10-08 09:48 | installed as `tv-arm-staging`; built from the same merged revision as dev. |
| trade-control | `v148-47-g47183b36` | 2026-10-08 09:48 | installed as `trade-control-staging` with `http://127.0.0.1:8788` baked in. Matching `tv-news`, `replay-candles`, `journal`, and shared worker operator tools rebuilt and installed. `journal-staging` displays broker-confirmed exit reasons and signed exit slippage. |
| backend | `staging` @ `47183b36` | 2026-10-08 09:48 | rebuilt and installed `trade-control-worker-staging`; service active and `GET /health` returned HTTP 200. Migration 7 applied. Verified one stop-loss report for order 2525 / trade 2526, no later reconcile warnings by insertion order, and the plan still in `await_entry`. |
| contract | `v3` | — | unchanged by v23 (recording is observe-only) |

### prod

| part | version | deployed (Brisbane) | notes |
|---|---|---|---|
| — | — | _not yet stood up_ | first promotion target: next Monday if staging is green |

---

## Reproducing an incident

1. Read the env's row above → note the `backend` tag and `contract`.
2. `git checkout <backend-tag>` (e.g. `v23`).
3. Pull the relevant recorded requests from that env's R2 bucket
   (`trade-control-recording-staging` / `-prod`), filtered by `trade_id`.
4. Replay them locally (see the roadmap's record/replay page) — the
   recorded body + headers are the exact inputs the worker saw.
