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
| **backend** | the Cloudflare Worker (this repo's `src/`) | `wrangler deploy` |
| **contract** | the signed wire format both speak | bumped only on wire-format change |

The version numbers do **not** move in lockstep — only the **contract**
must stay compatible across a deploy. Bump `contract` only when the
message wire format changes.

## Branch → environment model

| branch | environment | worker name | deploy rule |
|---|---|---|---|
| `main` | dev (today's existing worker) | `trade-control-web-hook` | deploy freely |
| `staging` | staging (demo account) | `trade-control-web-hook-staging` | deploy freely; must run **1 week unchanged + profitable** to promote |
| `prod` | prod (live account, later) | `trade-control-web-hook-prod` | only promoted-from-staging code |

Each branch carries its own `wrangler.toml` (own worker name, KV
namespace, R2 bucket) so a plain `wrangler deploy` on a branch targets
that environment — no `--env` flag to forget. Isolation is total: separate
worker, separate KV, separate R2.

**Promotion (staging → prod), Mondays:** if staging ran a full week with
no code changes and turned a profit, merge `staging` → `prod`, set the
prod `wrangler.toml` pointers, deploy prod, then cut a fresh `staging`
from `main` carrying the week's accumulated changes.

---

## Current state

<!-- Update the cells below on every deploy/promote. Dates are Brisbane. -->

### staging

| part | version | deployed (Brisbane) | notes |
|---|---|---|---|
| pine | `candle-signals-v2` | — | manual; record the TV version note |
| tv-arm | `0.1.0` | — | |
| trade-control | `0.2.0` | — | |
| backend | `v23` | _pending first deploy_ | first recording-enabled build |
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
