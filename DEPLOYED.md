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
| tv-arm | `0.1.0` | 2026-07-30 | installed as `tv-arm-dev` (dev URL + `Candle Signals v25` baked) |
| trade-control | `0.2.0` | 2026-07-30 | installed as `trade-control-dev` (dev URL baked) |
| backend | `main` @ `ab96b20` (`v122`) | 2026-07-30 | Version `v122`. **v119** news pause now pulls resting orders (was: blocked new entries only, so pendings filled on the news spike). **v120** those reasons became a shared refcount of named holders (`core::hold`) so a spread hour and a news pause overlap and lift independently. **v121** a failed `cancel_order` is classified via `lookup_attempt_state` instead of swallowed — a vanished (filled) order is pruned so the OFF side can't re-place it. **v122** `SpreadBlackoutRecord` → `HeldTradeRecord`; **DB migration `0005`** renames the table (rows preserved, no data migration). |
| contract | `v3` | — | unchanged by v24/v25 (`open` is optional) |

### staging

| part | version | deployed (Brisbane) | notes |
|---|---|---|---|
| pine | `v2.4` (study title `Candle Signals v24`) | — | pinned to the pre-`open` version; chart **unchanged** this deploy. v25 worker degrades gracefully when `open` is absent (rides baked geometry). `tv-arm-staging` bakes this study title. |
| tv-arm | `0.1.0` | 2026-07-30 | installed as `tv-arm-staging` (staging URL + `Candle Signals v24` baked) |
| trade-control | `0.2.0` | 2026-07-30 | installed as `trade-control-staging` (staging URL baked) |
| backend | `staging` @ `bef262a` (`v122-1`) | 2026-07-30 | Merged `main` v119–v122: news pause pulls resting orders; shared hold refcount; failed-cancel classification; `HeldTradeRecord` rename. **DB migration `0005` applied** — table renamed in place, all 12 live plan states / 4 pauses / 3 order bodies survived the restart (hold table was empty, so nothing in flight to preserve). **Promotion-gate week restarts from this deploy.** |
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
