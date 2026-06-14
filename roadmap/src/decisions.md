# Decisions & rejected ideas

A record of design decisions and, importantly, ideas we **considered and
rejected** — so they don't get re-proposed without remembering why.

## Rejected: single deployment serving both live + staging

The idea was: one worker binary, one deployment, that processes each message for
both live and staging — using frozen sub-crate versions for the "stable" half
and latest versions for the "staging" half, with a `match version { ... }`
dispatch inside.

**Rejected.** It fights us at exactly the wrong time. The killers:

1. **Two logical systems, one address space, one KV namespace.** When live and
   staging both process an alert and one has a bug, they corrupt each other's
   state unless *every* KV key is perfectly partitioned by environment. We
   already have a history of KV key-scoping bugs (vetos/cooldowns/preps
   colliding across accounts). Adding a live/staging axis to that fragile
   keyspace risks a cross-environment incident **on the live account** — the one
   place it's unaffordable.

2. **"Stable gets continually redeployed alongside staging — theoretically
   shouldn't do anything."** On a live-money worker that sentence precedes every
   incident. Every staging deploy becomes a live deploy; the whole point of a
   frozen stable is lost.

3. **Cargo's versioning robustness doesn't help here.** Cargo solves
   *build-time* dependency resolution. The actual problem is *runtime* routing
   of a message to different behaviours — a `match version` dispatch, which is
   feature-flags in a costume. We'd pay the cost of feature flags **plus** a
   private crate registry.

**Chosen instead:** the [proxy / fan-out web-hook](./proxy.md) — two separately
deployed workers, separate KV namespaces, separate accounts, with a thin async
proxy fanning one alert out to both. ~90% of the wanted benefit, none of the
shared-state risk.

## Decided: typed events, not a code dictionary

For correlation/journaling we use a typed `event_type` enum serialized to JSON,
not an intent/reply code-lookup dictionary. A dictionary rots and lives apart
from the data; a typed enum is self-describing and greppable. See
[Event schema](./event-schema.md).

## Decided: record/replay before going live

Sequencing: build the confidence engine (record/replay + simulator) first, then
go live on demo-proven code, then proxy and the rest. The blocker to go-live
should be a replay-verified profitable week — nothing else.

## Decided: account routing in the message, not KV

Routing policy (`live_account` / `demo_account`) goes in the signed message so
it's auditable in the recording and replay. KV is for runtime state, not policy.
See [Multi-account routing](./multi-account.md).

## Decided: R2 for the durable archive

The Cloudflare object store is **R2** (not "S2"). Zero egress, generous free
tier, per-message JSON objects. KV is only a short rolling buffer.

## The standing architectural rule

> Every feature's **decision** is a pure function of `(message, KV state)` in
> `core/`. Only the **KV read/write and broker call** live in the worker.

This is what makes every feature replay-testable by construction. A feature that
smuggles a decision into the worker's I/O layer becomes untestable — don't.
