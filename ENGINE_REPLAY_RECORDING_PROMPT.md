# Prompt: record replayable tick-bundles from the internal engine

Paste everything below the line into the `trade-control-web-hook` Claude (on
`main`). It is a **design + planning** task: scope and propose (then, on
approval, build) recording of the new cron-driven engine's per-tick inputs and
outputs to R2, so an engine tick can be replayed deterministically against a
mocked broker. The worker's own Claude knows the internals best — this prompt's
job is to pin down *what changed about the recordable surface* after the
rearchitecture, reconcile it with the existing `roadmap/` design, and define the
bundle contract.

---

## Context: the recording target moved

The system used to receive **inbound signed TradingView alerts** over HTTP, and
`src/recording.rs` recorded one `RequestRecord` per inbound request (body +
headers + `logs[]` + status/outcome) to R2. A downstream tool
(`trading-tax-tracker`) consumes those R2 records, and there's now a per-trade
**playback bundle** format + a replay-harness prompt
(`BUNDLE_REPLAY_PROMPT.md` in this repo) built around replaying those recorded
HTTP requests against a mock broker.

**That model no longer matches the architecture.** After the rearchitecture
(engine Stages A–F, `engine/` crate, `src/cron/engine.rs`):

- TradingView alerts are **gone** as a system input. `tv-arm` now annotates the
  chart, reads the operator's levels, builds a **`TradePlan`**, and HTTP-pushes
  it once via `Action::Register` (`--register-plan`). The plan persists in KV
  (`plan:{scope}:{trade_id}`).
- The worker **generates the alerts internally.** A cron tick
  (`run_engine_tick` → `tick_one`, every ~15 min) loads each registered
  `TradePlan` + its prior `PlanState` from KV, pulls fresh **candles** from the
  broker, runs the **pure** `evaluate_plan(...)`, and dispatches the
  `FiredIntent`s it returns (placing broker orders), then persists the new
  `PlanState`.

So the thing worth replaying is no longer "an HTTP alert body". The existing
`RequestRecord`/R2 recording still fires for the *register* POST and any
remaining control actions, but **the cron tick records nothing to R2 today** —
and the tick is where all the trading decisions now happen.

## The new recordable surface (what a tick replay needs)

A cron engine tick is a pure function of typed inputs, exactly like the old
alert dispatch was. The replay tuple is:

```
evaluate_plan(plan, prior_state, new_candles, detector_window, now, expires_at)
  → PlanEval { fired: Vec<FiredIntent>, new_state: PlanState, done: bool }
```

Everything `evaluate_plan` reads is already typed and (mostly) serializable, and
it's **pure** — no I/O, no hidden state. That's the same gift the roadmap's
record-replay design leans on ("a replay is fully determined by `(KV snapshot,
input, recorded broker responses)`"), just relocated from the HTTP edge to the
cron tick. The recordable input per tick is what the operator already identified:

1. **`TradePlan`** — the static plan (`trade_id`, `instrument`, `granularity`,
   `direction`, `pip_size`, `rules[]`, `shadow`). Pull it from the tick, don't
   re-derive it.
2. **Per-tick market data** — the `new_candles` (broker `get_candles`, closed-
   only, ascending, MID) **and** the `detector_window` (the wider H&S Pine back-
   window). Both are needed: `new_candles` drives the FSM, `detector_window`
   drives the pattern latch. This is also exactly what the
   [broker simulator](roadmap/src/broker-simulator.md) needs to simulate fills,
   answering that doc's open question ("where do candles come from during
   replay") — they're recorded *in the tick bundle*, not refetched.
3. **KV per tick** — the prior `PlanState` read in, and the new `PlanState`
   written out (plus any `mark_seen` / clear writes the dispatch did). This is
   the roadmap's `KvTransition` idea, scoped to the tick: `(key, before, after,
   success, error)`. Record reads too, so a touched-only snapshot is sufficient.
4. **Cron tick timing** — `now` (the tick instant) and `expires_at`. These are
   load-bearing: `evaluate_plan` takes `now`/`expires_at` and the FSM's
   watermark/expiry logic depends on them, so a replay must restore the exact
   tick clock, not use wall-clock.

The recordable **output** per tick (the golden snapshot to assert a replay
against) is the `PlanEval`: the `fired` intents (each `FiredIntent` =
`rule_id` + `intent` + `candle` + optional `signal`), the `new_state`, the
`done` flag, plus the **dispatch outcomes** (what the broker did with each fired
intent — fill/order-id/error, or the shadow "would-fire" log). For shadow plans
(`plan.shadow == true`) the dispatch is observe-only, which makes shadow ticks
the *safest possible* thing to record first — they touch no broker.

## What I want from you

A **plan** (and, on approval, the implementation) for recording engine ticks to
R2 as replayable **tick-bundles**, plus the path to replay one. Specifically:

1. **Reconcile with the roadmap.** Read `roadmap/src/record-replay.md`,
   `event-schema.md`, `broker-simulator.md`, `recordability-audit.md`. The
   roadmap's `WorkerEvent` enum, `KvTransition{success,error}`, correlation keys
   (`trade_id`/`id`/`request_id`/`seq`/`fire_seq`/`ts`), and broker simulator are
   all still the right vocabulary — but they were drafted for the alert-dispatch
   path. Tell me concretely how the **cron tick** maps onto that schema: what a
   tick's `request_id`/`seq` mean (a tick is one "invocation" spanning N plans —
   is each plan a separate correlation chain?), which `WorkerEvent` variants a
   tick emits (`GateDecision`? `KvTransition`? `BrokerCall`/`BrokerResponse`?
   `OrderPlaced`?), and whether the tick-bundle is the same artifact as the
   roadmap's per-`intent_id` event stream or a sibling. Don't reinvent what the
   roadmap already decided; extend it.

2. **Define the tick-bundle format.** A self-contained, serde-round-trippable
   JSON object per recorded tick (or per `(tick, plan)` — you decide and justify),
   carrying inputs (1–4 above) + outputs, with stable correlation keys. Keep it
   **WASM-safe** (the recording runs in the worker) and **pure-typed** (lives in
   `core/` or `engine/`, unit-tested with serde round-trip). Watch for the same
   trap the existing bundle hit downstream: a type that's `Serialize`-only won't
   round-trip — audit `TradePlan`/`PlanState`/`Candle`/`FiredIntent`/`Intent`
   for `Deserialize`, and add derives where missing (these are *our* types, so we
   can).

3. **Wire the recording into `run_engine_tick`/`tick_one`, fire-and-forget.**
   Record via `ctx.wait_until` off the response/critical path (the roadmap's
   firm constraint — recording must never add latency or break trading; the
   existing `record_to_r2` is the pattern). Fail-soft on every axis (missing
   bucket binding, serialize error, put failure → log + swallow). Decide the R2
   key layout — extend the roadmap's `r2://trade-archive/...` scheme; a tick is
   naturally keyed by `(tick_ts, trade_id)` or under `events/<trade_id>/`.
   **Bucket names are environment-specific and get renamed — never hardcode a
   bucket name; read it from the binding/config** (this has bitten before).

4. **Order of build (lowest-risk first).** I'd expect: (a) the pure tick-bundle
   type + serde round-trip test in `core/`; (b) record shadow ticks only (no
   broker, safest) behind a flag; (c) extend to live ticks; (d) the replay path —
   a native CLI `replay` that restores a tick-bundle's plan+state+candles, runs
   the same `evaluate_plan`, and diffs `fired`/`new_state`/`done` against the
   recorded `PlanEval` (the broker simulator from the roadmap fills pending
   orders from the recorded candles). Confirm or revise this ordering.

5. **Don't break the existing R2 consumer.** `trading-tax-tracker` reads the
   current `RequestRecord` shape from R2 (`req/<date>/<ts>-<request_id>.json`).
   Tick-bundles are a **new, distinct** object kind — put them under a different
   prefix so the existing reader's `parse_request_records` never trips over them,
   and tell me the prefix so I can teach the downstream tool to read tick-bundles
   too (a sibling to its current `bundle` command).

## Constraints (carry these through)

- The signed intent/register wire format does **not** change — this is
  worker-side observability (roadmap's firm constraint).
- Engine stays pure (`engine/`, `core/` evaluate path: no I/O, no `#[cfg]`).
  Recording I/O lives only in the worker glue (`src/`), exactly as `evaluate_plan`
  is pure and `tick_one` does the I/O.
- WASM-safe in `core/`; no `ring`-pulling deps added to the worker build.
- 2024 edition, no `mod.rs`, no `unwrap`/`expect` outside tests, `color_eyre`/
  `Result` at the I/O edge, `tracing`/`rlog!` for logs, one concept per module,
  inline format args.
- Fail-soft recording; `ctx.wait_until`; never hardcode a bucket name.
- You're on `main` (dev). Don't deploy `staging`/`prod`. Gate before commit:
  `cargo test` (workspace), `cargo clippy --all-targets -- -D warnings`,
  `cargo fmt`, and a build of the wasm worker. Per-crate `vNN` tag + CHANGELOG
  when green, per the repo policy.

## Deliverable from this prompt

First: a written **plan** — the tick-bundle schema (fields + correlation keys +
how it maps onto the roadmap's `WorkerEvent`/`KvTransition`), the R2 key layout
and prefix, the `run_engine_tick` wiring sketch, the build order, and the replay
path. Flag any `Serialize`-only types that need a `Deserialize` derive. Then, on
my approval, build it shadow-first.

### One thing to confirm with me before building

Is the unit of recording **one tick** (all plans evaluated in that cron run, one
bundle) or **one `(tick, plan)`** (one bundle per plan per tick)? I lean toward
per-`(tick, plan)` — it matches the roadmap's "the natural unit is the
position/setup, not the fire", keys cleanly by `trade_id`, and lets a single
trade's whole life be replayed by globbing its prefix — but you have the cron
loop in front of you, so call it.
