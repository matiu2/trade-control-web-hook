# Phase 0 — implementation notes (from the real code)

Findings from reading the actual worker (`src/`, `core/`) on 2026-06-14, to turn
the [event-schema](./event-schema.md) open questions into facts before any code
lands. Everything here is **verified against source** with `file:line`.

> **Caveat for a future reader:** these line numbers drift as the code changes.
> They were accurate on 2026-06-14. Treat them as a starting point, not gospel —
> re-grep the symbol name if a line doesn't match.

## What's already true (good news)

### Gates are mostly already pure → `GateDecision` emits cleanly

The standing architectural rule ("decision is a pure function, I/O at the edges")
already holds for most gates:

| gate | decision function | pure? |
|---|---|---|
| allow_entry | `allow_entry_gate.rs` `evaluate()` → `AllowEntryOutcome` | **pure** (Rhai + scope, no I/O) |
| allow_close | `allow_close_gate.rs` `evaluate()` → `AllowCloseOutcome` | **pure** |
| candle (golden/confirmed) | `candle_gate.rs` `evaluate()` → `CandleGateOutcome` | **pure** |
| too_close | `too_close.rs` `market_replace_plan()` → `TooClosePlan` | **pure** |
| spread_blackout | `spread_blackout.rs` `spread_blackout_decision()` → `bool` | **pure** (I/O in `run_enter`) |

For these, the worker can emit a `GateDecision` event by recording the returned
outcome value — no refactor needed.

The remaining gates are **I/O-only or tangled** — the decision is trivial but
async, or baked into the evaluation:

| gate | where | shape |
|---|---|---|
| cooldown | `lib.rs` `is_cooled_down()` | I/O-only (bool from a KV read) |
| veto | `lib.rs` `is_vetoed()` loop | I/O-only (bool per KV read) |
| prep | `lib.rs` `get_prep()` loop | tangled (timestamp rule + KV reads) |
| retry | `retry_gate.rs` `evaluate()` | tangled (KV reads + `lookup_attempt_state` broker call) |

For these, emitting `GateDecision` means recording the *result* at the call site
(still fine — we don't need the decision pre-extracted, just observed).

### `mark_seen` is already correct (the 2026-06 fix is in place)

`seen_decision()` (`lib.rs:344`) returns `Mark` **only** for `ActionResult::Ok`;
`Failed` and `Rejected` return `Skip`. Verified verbatim. So the replay-protection
semantics the schema relies on are already as documented — failures don't poison
the id. The `ReplayGuarded` event maps to the 409 path; it is *not* an error.

### Broker ids are already captured

`EntryAttempt` (stored under `entry_attempt:{scope}:{trade_id}:{n}`) already holds
`broker_order_id` and `broker_trade_id`. `place_entry` returns the order id
(`lib.rs:1460`, `:1605`); `set_entry_attempt_broker_trade_id` snapshots the
position id once a lookup finds an open position (`retry_gate.rs:247,280`). So
**`OrderPlaced` and `PositionOpened` already have their ids at the seam** — the
event just needs to emit what's already in hand.

### `ActionResult` → status code mapping (for `OutcomeRecorded`)

`ActionResult` (`lib.rs:409`) has three variants → response mapping at `lib.rs:229`:

- `Ok(_)` → **200**
- `Failed(_)` → **502** (broker reached, call failed)
- `Rejected { response, .. }` → the embedded response: **400** (validation),
  **409** (already-seen / prep-expired), **412** (gate), **423** (paused /
  cooled / out-of-range / spread-blackout), **500** (state/KV error), **503**
  (broker login).

`OutcomeRecorded` should carry both the raw status and the decoded variant.

## What the schema page got WRONG (corrections)

### There is **no** `request_id` today

The schema page assumed the worker already emits a per-request id like
`a0a6a1fd99750ba9`. **It does not.** `tracing_console.rs:48` hands out monotonic
span ids that are *internal to the subscriber only* — never emitted to logs, never
used for correlation. The `a0a6...` in CLAUDE.md was an illustrative example, not
a real field.

**Consequence:** `request_id` is a **new thing to add** in Phase 0 — mint one at
the `fetch` entry point and thread it through. It's cheap, but it's not free as
the schema implied. Until it exists, intra-invocation grouping falls back to
`(id, seq)`.

### The id fields are `id` and `trade_id`, not `intent_id` / `correlation_id`

The intent (`core/src/intent.rs`) carries:

- **`id: String`** (line 308) — unique per intended transaction; the replay-dedup
  key. This is what the schema calls `intent_id`.
- **`trade_id: Option<String>`** (line 491) — the grouping slug shared by all
  alerts of one setup (the H&S 5-alert bundle, multi-shot, reversal). This is
  what the schema calls `correlation_id`.

There is **no** separate `intent_id` field and **no** `correlation_id` field.

**Recommendation:** the schema should map onto the real names to avoid a
translation layer:

| schema name | real field | notes |
|---|---|---|
| `correlation_id` | `trade_id` | **`Option`** — absent on control actions (see below) |
| `intent_id` | `id` | always present |
| `request_id` | *(new)* | mint at `fetch` entry |
| `seq` | *(new)* | monotonic per request |
| `fire_seq` | *(derivable)* | from the `entry_attempt:` list count + `seen-retry:` keys |

### `trade_id` is absent on control actions — the open question is answered

The schema asked "does `correlation_id` exist for control actions?" **Answer:
no, and that's correct by design.**

- **Carry `trade_id`:** `Enter`, `Veto`, `ClearVeto`, and `Close`/`NewsStart`/
  `NewsEnd`/`Pause`/`Resume` when they operate on a trade/window.
- **Instrument-scoped only (no `trade_id`):** `Prep`, `ClearPrep`, `Invalidate`,
  `Status`, `Unlock`, `PrepExpire`.

So events for control actions will have `correlation_id = None`. They correlate by
`(account, instrument)` + `id`, not by trade. The schema should treat
`correlation_id` as `Option`, not assume-present.

## KV recording: the wrapper target

All KV goes through `KvStateStore` (`src/state/kv.rs`) implementing the
`StateStore` trait (`core/src/state.rs`). **That trait is the single wrap point**
for `KvTransition` recording — wrap the impl, not the call sites. Keyspaces
(verbatim formats) are inventoried in [KV & broker seams](./seams.md).

One nuance for replay determinism: several keyspaces keep an advisory
**`index:*`** list (seen, cooldowns, preps, vetos, prep-blocks) alongside the
primary keys. A KV snapshot for replay must include these indices or `status`
reconstruction differs.

## Net effect on Phase 0 scope

- **Smaller than feared:** gates already pure, `mark_seen` already correct, broker
  ids already captured, one clean trait to wrap for KV.
- **Two genuinely new things:** mint a `request_id` at entry; add a per-request
  `seq` counter. Both small, both in the worker glue, neither touches intents.
- **One naming decision:** adopt `id` / `trade_id` as the canonical event keys
  (with `trade_id` optional) rather than inventing `intent_id` / `correlation_id`.
