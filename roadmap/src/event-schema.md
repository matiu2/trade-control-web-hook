# Event schema & correlation ids

**Status:** design informed by consumers · **Phase:** 0 (foundation) · **Size:** small, design-only

The shared vocabulary for recording, replay assertions, and journaling. Get it
right once; everything downstream depends on it.

## Three reasons we record (in priority order)

1. **Debug** — *the current driver.* Find what the worker actually did, replay
   it, see a fix change behaviour in seconds.
2. **Track edge / viability / profitability** — later; grown slowly.
3. **Understand & educate (self + others)** — later; folded into
   [journaling](./journaling.md), LLM-assisted, not automated.

The schema below is **debugging-first**. The richer fields the two downstream
consumers asked for (the timeline-reconstruction LLM and the journaling LLM)
serve reasons 2 and 3 — they're real and recorded under
[Later](#later-edge--journaling-automation) so the input isn't lost, but they
are **not** Phase 0.

## Two firm constraints

- **The intents stay as-is.** The current signed intents encode the per-trade
  rules in an already-understandable form. This schema is **worker-side
  observability** — it records what the worker *does*, and requires **zero
  changes to the wire format**. Don't bake geometry, designed-plan numbers, etc.
  onto the intent.
- **The chart is the journal substrate.** The operator annotates intent onto the
  chart at arm time and reads it back when journaling. Full pattern geometry is
  drawn *after* the trade, by hand — it is **not** an arm-time fact and does not
  belong in the event stream (see [journaling](./journaling.md)).

## Decided: typed events, not a code dictionary

A **typed `event_type` enum serialized to JSON** — not an intent/reply
code-lookup dictionary, not just "more verbose messages".

- A code→meaning dictionary rots and lives apart from the data.
- A typed enum is **self-describing and greppable** — the meaning travels with
  the record.

---

## Phase 0: the debugging-first subset

Everything here is worker-side; none of it touches the intents.

### Correlation keys (on every event)

Both consumers independently said the natural unit is the **position/setup**, not
the fire — one setup spawns N fires (multi-shot, degraded refill, reversal). So
`trade_id` alone is insufficient.

| key | scope | role |
|---|---|---|
| `correlation_id` | one setup / position | aggregate-by key (journal entry) |
| `intent_id` | one fire (one alert firing) | the fire key |
| `request_id` | one worker invocation | causal chain within a fire (worker already emits this, e.g. `a0a6a1fd99750ba9`) |
| `seq` | monotonic within a request | **intra-fire ordering** — wall-clock can't (events share a second; the post-close tail races) |
| `fire_seq` | monotonic per `correlation_id` | which fire (fire 1 placed; fires 2–N were 409 no-ops) — a fact, not inferred from spacing |
| `ts` | UTC RFC3339 + offset | cross-fire / cross-stage ordering; never localize at emit |

Optional `parent_event_id` makes causality explicit (`BrokerCall` → its
`GateDecision`; `BrokerResponse` → its `BrokerCall`) instead of inferred from
adjacency.

### Variants (debugging-essential)

| variant | carries | why it matters for debugging |
|---|---|---|
| `AlertReceived` | raw signed body, headers, source | the input to replay |
| `GateDecision` | gate name, passed/rejected, **reason code** | why an action was/wasn't taken |
| `KvTransition` | key, `from_state`, `to_state`, **`success: bool`, `error`** | **the highest-value debug record** — surfaces intent that *didn't* execute (Bug #6: wanted to cancel, couldn't) |
| `BrokerCall` | method, args, `parent_event_id` | what we asked the broker |
| `BrokerResponse` | fill price / order id / error | what the broker said (incl. the 502 path) |
| `OrderPlaced` | `broker_order_id` at the moment known | load-bearing join key |
| `PositionClosed` | `broker_position_id`, `exit_type`, exit price/time | what actually happened, even on a stop-out the worker didn't initiate |
| `OutcomeRecorded` | decoded outcome (Ok / Failed / Rejected) + `status_code` | the dispatcher result |
| `ReplayGuarded` | — | the 409 "recognized a refire, deliberately did nothing" — **not** an error |
| `IntentExpired` / `PayloadParseError` | distinct types | both are 400s today — splitting them removes a known disambiguation trap |

Two emphases both consumers stressed:

- **`KvTransition` records failures.** `success` + `error` make "the bot wanted
  to X but couldn't" a queryable, first-class record — never a buried error
  string. This is the single most useful thing for debugging.
- **`PositionClosed` is authoritative.** TradeNation's closed-trade row is
  **id-less** (instrument + open-price + open-minute + P&L only). So *our* close
  event is the source of truth and the broker row is a cross-check. Emit it
  **even for worker-uninitiated stop-outs** — those swallowed-cancel stop-outs
  are exactly the cases that are otherwise unreconstructable.

### Phase 0 acceptance

- [ ] `WorkerEvent` enum in `core/` (replay-testable, WASM-safe).
- [ ] Stable JSON via serde; round-trips in a unit test.
- [ ] Every event carries the six correlation keys + optional `parent_event_id`.
- [ ] `KvTransition` carries `success` + `error`.
- [ ] `PositionClosed` emitted even on worker-uninitiated stop-outs, with broker
      ids.
- [ ] `ReplayGuarded`, `IntentExpired`, `PayloadParseError` are distinct types.
- [ ] No change to the signed intent wire format.
- [ ] Documented in the README event-format section.

---

## Later (edge + journaling automation)

Recorded for when reasons 2 and 3 get built out. Not Phase 0. None of this
should expand the intents — where a fact originates off-worker (geometry, news),
it's captured by its own producer, not bolted onto the wire format.

### Edge / profitability (reason 2)

- **`PositionClosed` FX fields** — `realised_quote`, `quote_ccy`,
  `realised_home`, `home_ccy`, `fx_rate`, `fx_rate_source_ref_id`: the actual
  dated rate the broker used. Kills the FX-pairing heuristic both consumers do
  by hand. (Or a separate `FxApplied` event — same fields.)
- **First-class terminal states with reason codes** — `filled`, `never-filled`,
  `rejected`, `veto-closed`, `expired`; `Skipped` (a rule fired — name it) vs
  `Missed` (incidental). Non-fills are the rule-validation dataset.
- **Designed-vs-filled split** — preserve intent's original numbers even after a
  degraded refill (real case: designed 4.66R, filled 1.15R).
- **MFE / MAE** — needs the intrabar price path (periodic mark-to-market events
  or a post-hoc candle join).
- **`OrderFilled` / `PositionOpened`** — `broker_position_id` + fill price/time;
  closes the "did the stop trigger, when" gap.
- **News context** — thread economic-calendar events keyed to the trade window
  so the blackout verdict is *derived*, not hand-filled.

### Educate / journaling (reason 3)

- Pattern geometry (7 H&S / 4 M/W anchors) is **operator-authored after close**,
  lives on the chart + in the `journal_overlay`, never in the event stream.
- Human overlay: a `journal_overlay` per `correlation_id` —
  `{ notes, lessons, screenshot_url, manual_field_overrides, exclude_from_stats }`,
  merged at render time so edits survive regeneration. See
  [journaling](./journaling.md).

## Open questions

- Does `correlation_id` exist for **control actions** (prep/veto/pause), or only
  entry-bearing paths? They may need a synthetic id.
- Where is `GateDecision` emitted — the pure helper or the worker wrapper?
  (Prefer: helper returns the decision, worker emits the event.)
- Snapshot *all* touched KV per request, or only keys the request reads/writes?
  (Touched-only is cheaper and sufficient if reads are recorded too.)
