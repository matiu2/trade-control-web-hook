# Record & replay

**Status:** not started · **Phase:** 1 (foundation) · **Size:** the big one

The confidence engine. This is the single highest-leverage item on the roadmap:
it collapses the bug-fix feedback loop from *a week* to *seconds*, which is what
makes going live safe.

## The problem it solves

Today: fix bug → wait a week on demo → hope similar price action recurs →
re-analyse → guess if fixed.

After: fix bug → replay recorded messages against the [broker
simulator](./broker-simulator.md) → see the behaviour change immediately, as a
test.

## Why our state model makes this clean

**All worker state lives in Cloudflare KV**, and Workers are stateless per
request. So a replay is *fully determined* by:

```
(KV snapshot, alert body, recorded broker responses)
```

There is no hidden in-process state to chase. That's a real gift — it means the
whole thing reduces to snapshotting KV.

## The key abstraction: a KV-recording wrapper

Wrap KV access so every read/write emits a `KvTransition`
(`key, value_before, value_after`) tagged with `intent_id` + `ts`. **One wrapper
serves three masters:**

1. **Deterministic replay** — restore `value_before` for all touched keys,
   replay the alert, assert `value_after` matches (or differs as the fix
   intends).
2. **Journaling** — the KV diff *is* the state-transition timeline.
3. **Post-mortem debugging** — see exactly what each fire touched.

Recording is **async, off the response path** (`ctx.waitUntil`), so it does not
add latency to the TradingView response. In fact this *improves* latency vs.
any synchronous logging today.

## Two replay paths (they catch different bugs)

Replay comes in two flavours. They are complementary — use the fast one in the
inner loop, the realistic one as an integration check.

### 1. Native CLI harness — fast, deterministic, the workhorse

A **native** CLI subcommand (not WASM — full access to instrument-lookup, the
simulator, everything):

```
trade-control replay <intent_id | trade_id>
```

Steps:
1. Pull events for the id from [R2](./proxy.md) (or local fixture).
2. Restore the recorded KV snapshot.
3. Feed the recorded alert(s) through the **same** `core/` gate logic.
4. Diff resulting broker calls + KV transitions against the recorded ones.

Sub-second, runs in CI, in-process simulator swap. Because it calls the same
pure `core/` functions the worker does, a passing replay is real evidence the
*decision logic* behaves the same.

**Limitation:** it exercises `core/`, **not** the worker wiring in `src/lib.rs`
— HMAC verification, the KV binding, `waitUntil`, request parsing, response
codes. The 2026-06 CHF/JPY incident (mark-seen-on-failure poisoning the intent
id) lived in `record_dispatcher_outcome` / `seen_decision` in the worker glue,
**not** in a pure helper — so the native harness alone would not have caught it.

### 2. `wrangler dev` replay — slower, but tests the *real* worker

`wrangler dev` runs the actual compiled worker in the local `workerd` runtime
with a **local KV simulation** (Miniflare-backed — reads/writes hit a local
store, not production). So we can:

1. Seed local KV with the recorded snapshot
   (`wrangler kv key put --local`, or the Miniflare persist dir).
2. POST recorded raw alert bodies at `http://localhost:8787` with the recorded
   headers.
3. Let the **real** worker parse, verify HMAC, hit local KV, run gates, and
   dispatch — then assert the response code + local-KV after-state.

This catches the class of bug the native harness can't: anything in the worker
glue. It's the path that would have caught the CHF/JPY mark-seen bug.

**The catch:** the worker is WASM and can't link the native simulator
in-process. So this path needs the broker reachable over **HTTP** — a tiny local
mock fed the recorded `BrokerResponse`s — rather than a trait swap. And KV must
be seeded with `--local` first or the replay isn't deterministic.

> **Rule of thumb:** native harness for the inner fix loop (hundreds of runs);
> `wrangler dev` replay as the pre-deploy integration check on worker wiring.

## Storage layout (R2)

```
r2://trade-archive/
  raw-alerts/<utc_ts>-<intent_id>.json     # signed inbound body verbatim
  events/<intent_id>/<utc_ts>-<type>.json  # one object per event
  kv-snapshots/<intent_id>-<utc_ts>.json   # touched keys, before-state
```

One object per event (not append-to-blob) → cheap writes, no read-modify-write
races between concurrent fires.

## Acceptance

- [ ] KV-recording wrapper emits `KvTransition` for all reads/writes.
- [ ] Recording is fire-and-forget via `waitUntil` (no added response latency).
- [ ] Events + raw alerts + KV snapshots land in R2.
- [ ] `trade-control replay <id>` (native) reconstructs and diffs a real fire.
- [ ] `wrangler dev` replay path: a script that seeds local KV from a snapshot,
      POSTs recorded alerts, and asserts response code + after-state.
- [ ] A local HTTP broker mock that returns recorded `BrokerResponse`s (for the
      `wrangler dev` path).
- [ ] A regression test exists: a recorded buggy fire, plus an assertion that
      the fixed code produces the intended different outcome.

## Open questions

- Snapshot *all* KV or only keys the request touches? (Touched-only is cheaper
  and sufficient if we record reads too.)
- For `wrangler dev`: seed KV via `wrangler kv key put --local` per key, or
  point Miniflare at a persist dir we write the snapshot into wholesale?
- Can the HTTP broker mock be shared between the `wrangler dev` path and a
  future integration test, or does each need its own?
- Retention: how long do raw alerts live in R2 before archival/expiry?
