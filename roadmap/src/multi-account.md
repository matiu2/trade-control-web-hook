# Multi-account routing

**Status:** not started · **Phase:** 4 · **Size:** small (mostly falls out of the proxy)

Send a trade to more than one account — e.g. a live account and a "reversals"
account.

## Mostly solved by the proxy

Once the [proxy](./proxy.md) exists, "send to live + reversals" is largely
*which downstream workers the proxy fans out to*, each pinned to its own
account. No per-message account field needed for that case.

## For genuine single-worker mirroring

Where one worker really must mirror to two accounts, put the routing **in the
message**, not in code or KV:

```yaml
live_account: live
demo_account: reversals
```

Rationale:

- **Over hard-coding:** keeps the worker stateless about account *policy*.
- **Over KV:** KV is for runtime *state*, not routing *policy*. Routing in the
  message means it shows up in the recording and the replay — auditable.

## Acceptance

- [ ] Decide which cases are proxy-fan-out vs. in-worker mirror.
- [ ] If in-worker: the enter intent honours `live_account` / `demo_account`.
- [ ] Account routing appears in recorded events (auditable in replay).

## Open questions

- Do reversals trades need *different* gate parameters, or just a different
  account? (If different gates, this is more than routing.)
