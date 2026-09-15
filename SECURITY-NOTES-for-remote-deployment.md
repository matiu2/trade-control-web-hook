# Security notes — things to fix BEFORE the worker runs remotely again

**Status:** notes only, nothing fixed. Written 2026-09-15.

## Why this file exists

The signing scheme (HMAC over the alert body, `SIGNING_KEY`) exists because the
worker **used to run remotely** — on Cloudflare, reachable from the internet,
where an unauthenticated request could place a real order. Today the worker runs
**locally** (`127.0.0.1:8787` / `:8788`, see CLAUDE.md "Runtime: fully local"),
so the trust boundary is the machine itself and these findings are largely
inert.

**That is temporary.** The Oracle Cloud host is the long-term target
(`SPIKE-oracle-findings.md`); when the worker is remote again, every finding
below becomes live. So they are recorded now, while the context that produced
them is fresh, rather than rediscovered under time pressure at deploy.

⚠️ **None of these are exploitable by a remote attacker today** — there is no
remote attacker. They are all "the stored state is trusted more than the wire
is", which only matters when the wire is hostile *and* when store access and
wire access have different threat profiles.

---

## 1. `TradePlan` is stored UNSIGNED — the engine fires rules from an unauthenticated row

**Verified 2026-09-15** (`core/src/trade_plan.rs` has no `sig` field;
`worker/src/pg.rs:1208 put_trade_plan_impl` writes `to_jsonb(plan)`).

The register envelope's HMAC is verified once at the HTTP edge
(`core::dispatch::control::handle_register`) and then **discarded**. What
persists is the plain `TradePlan` JSONB. The engine then reads and fires those
rules on every tick with no integrity check.

**Why it matters remotely:** anyone who can write the `trade_plan` row can
author *all of a trade's future rules* — entries, vetos, closes — without ever
presenting a valid signature. That is strictly more powerful than forging a
single alert, because it authors an ongoing instruction stream rather than one
action.

**Why it is not a bug today:** the DB is local and access to it implies access
to the machine (and therefore to `SIGNING_KEY` itself). The signature would add
nothing against that adversary.

**Shape of a fix:** persist the verified envelope's signature alongside the plan
and re-verify on read, or store the plan as the signed bytes and parse on read
(what `order:{id}` bodies do). Note the second form is what
`fix/engine-path-order-body` adopts for order bodies — the same shape, applied
to a different row.

**Load-bearing consequence for reviewers:** the common defence *"the plan was
signed at arm time, so re-deriving an intent from it inherits that authority"*
is **FALSE at rest**. It is true only of the wire message. Don't build a
security argument on it (this note exists because that argument was made and
had to be withdrawn).

---

## 2. `park_stored_entry` stores an UNSIGNED re-serialisation

**Found 2026-09-15, not fixed** (`core/src/dispatch/enter.rs:~1193`).

On the engine path it stores `serde_yaml::to_string(&verified.intent)` — no
signature. `SignedBodySource` rejects that with `MissingSig` → `Unrecoverable`.

This is primarily a **correctness** bug, not a security one: engine-path parks
are likely unpromotable, so `order_control::promote` / `reprice` are blocked
there. Its comment claiming "re-serialising it loses nothing a promotion needs"
looks wrong. **Deserves its own investigation** — see the TODO list.

Recorded here because the *fix* has a security dimension: whatever makes that
body recoverable should sign it, the same way
`fix/engine-path-order-body` signs the `order:{id}` body, rather than adding a
second unsigned-but-trusted path.

---

## 3. Worker-side re-signing — accepted, with its reasoning

`fix/engine-path-order-body` (branch, unmerged as of writing) has the worker
**re-sign** a body it reconstructed from a stored plan, because the engine never
had signed bytes and `parse_and_verify` requires a `sig`.

**Judged acceptable, and it is a net improvement rather than a concession** —
precisely *because* of finding #1. The `order:{id}` row gains a tamper check it
did not have, and it is not the weakest link, since the `trade_plan` row it
derives from is unsigned and strictly more powerful.

Confinement that must be preserved if this is refactored:

- `resign` takes a **`Verified`** — the type `parse_and_verify` *produces* — so
  it structurally cannot launder untrusted bytes into a signature.
- Output goes **only** to `put_order_body`, for an order the broker already
  accepted.
- `id` / `not_after` ride through unchanged, so a re-signed body cannot outlive
  what the operator authorised.
- A worker-minted signature must **never** be accepted on the inbound webhook
  path.

⚠️ **The YAML rendering is security-relevant, not cosmetic.** The HMAC scans
**top-level `key: value` lines only** (see the
`[[signed_body_is_top_level_lines_only]]` memory). A mutation test caught this:
block-style YAML emits `entry:` then `  type: stop` on an indented line, so the
**trigger price vanishes** from the signed scan — the body verifies while
carrying a *different entry than was placed*. Nested values must be rendered in
**flow style on one line**. If that renderer is ever replaced, re-run that
mutation.

---

## Checklist before going remote

- [ ] Fix #1 — plans must be integrity-checked at rest, or the engine must not
      trust them.
- [ ] Investigate #2 and sign whatever it stores.
- [ ] Re-audit: what else is read from the store and acted on without a check?
      (`prep`, `veto`, `pause` rows, `HeldTradeRecord`, `EntryAttempt` —
      **none of these were audited**; this file lists only what was found
      incidentally while fixing something else.)
- [ ] Confirm `ADMIN_KEY` / `SIGNING_KEY` handling for a remote host (env vars
      on a local process today).
- [ ] Threat-model the DB itself: remotely, DB access and machine access may
      come apart, which is exactly the assumption every finding above relies on.
