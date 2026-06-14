# The proxy / fan-out web-hook

**Status:** not started · **Phase:** 3 (post-go-live) · **Size:** small worker

A thin separate worker that fans one TradingView alert out to **both** a live
and a staging worker, and records every inbound message centrally. This is the
*boring, correct* version of the "run live + staging at once" idea — see
[Decisions](./decisions.md) for why the clever single-deployment version was
rejected.

## What it does

1. Verify the HMAC once (reject junk early).
2. **Async fire-and-forget** `fetch()` to both live and staging workers.
3. Record the inbound message to [R2](./record-replay.md).
4. Return `200` to TradingView immediately — does **not** wait for downstreams.

## What it buys us

- **One TV alert → both systems.** Solves the ~100-alert TradingView limit and
  removes the need for two trading accounts just to test Pine changes.
- **Latency.** Proxy returns as soon as it's fanned out; live and staging run
  concurrently; the slow path is never on TV's response.
- **Two *separately deployed* workers.** Live stays frozen and untouched while
  staging redeploys 5×/week. Separate code, separate KV namespaces, separate
  accounts → blast radius contained. This is the real isolation; the
  shared-binary version could not give it.
- **Central recording point** for replay and journaling.

## Versioning / feature routing

Stamp a `min_version` or feature tag **in the message**, and let each downstream
decide to process / process-differently / ignore. Routing lives in the *message
and the downstream*, **not** in a shared binary. A single alert may be handled
the same by both, differently by each, or ignored by one.

## Storage: R2 (not "S2")

- The Cloudflare object store is **R2** (S3-compatible). "S2" doesn't exist.
- Free tier: ~10 GB storage, free Class A/B operation allowances, **zero egress
  fees** — ideal for an append-style per-message JSON archive.
- KV can hold a short rolling buffer of recent messages, but R2 is the durable
  home.

## Acceptance

- [ ] Proxy worker verifies HMAC, fans out to two configured downstreams.
- [ ] Fan-out is concurrent and does not block the TV `200`.
- [ ] Inbound messages archived to R2 with the standard event schema.
- [ ] Downstreams keyed by env: separate KV namespaces + accounts.
- [ ] A message can carry a version/feature tag that downstreams honour.

## Open questions

- Does the proxy wait for *neither* downstream, or for live only (so a live
  failure is at least visible in the proxy response)? Leaning neither — TV only
  needs the 200, and failures are recorded.
- One proxy worker, or does the proxy itself become the place we eventually
  retire by folding fan-out into a Durable Object? (Not now.)
