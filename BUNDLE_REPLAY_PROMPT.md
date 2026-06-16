# Prompt: replay trade bundles for mock-broker testing

Paste everything below the line into the `trade-control-web-hook` Claude. It is
self-contained: it describes the bundle file format, which parts are
load-bearing for *this* worker, the replay model, and the gotchas to respect.

---

## Task

`trading-tax-tracker` produces a **trade bundle** — one JSON document per trade
key that captures everything known about a single trade across all sources. I
want you to build a **replay test harness** in this repo that reads a bundle,
feeds the recorded requests back through the worker's own request handler with a
**mocked broker**, and asserts the worker reproduces the recorded behaviour.

This turns real captured trades into deterministic, offline regression tests —
no Cloudflare, no live broker, no TradingView.

## The bundle format

A bundle is a single JSON object. Its full shape (produced by
`trading-tax-tracker-cli bundle --trade-key <key> --out <file>`):

```jsonc
{
  "trade_key": "hs-us-2000-c1",
  "account": "reversals",                 // null if the tax tool wasn't given --account
  "inputs": {
    "tv_alerts":    [ /* TradingView alert rows  */ ],
    "cf_events":    [ /* Cloudflare log events    */ ],
    "r2_records":   [ /* RequestRecord objects    */ ],   // <-- THE LOAD-BEARING PART FOR YOU
    "broker_fills": [ /* normalised BrokerFill rows from the tax tool */ ]
  },
  "expected": {
    "ledger":        [ /* tax-tool LedgerRow   */ ],   // the tax tool's view, not yours
    "exceptions":    [ /* tax-tool ExceptionRow */ ],
    "verdict":        "...",
    "verdict_label":  "..."
  }
}
```

**For replaying the worker, only `inputs.r2_records` matters.** The other
fields are the *tax tracker's* downstream interpretation (TV alerts as it pulled
them, CF logs, the normalised fills it reconciled, and its own ledger/verdict).
They are useful context and can seed assertions, but they are **not** the
worker's input contract — the worker only ever saw an HTTP request.

### `r2_records[]` — these ARE your recorded requests

Each element is **field-for-field your own `recording::RequestRecord`** (the
tax tool deserializes the exact JSON your `record_to_r2` writes to R2). Shape:

```jsonc
{
  "ts": "2026-06-15T07:51:39.123Z",        // RFC3339, when you received it
  "request_id": "a1b2c3d4e5f60718",        // your FNV(body+headers) — NOT $workers.requestId
  "method": "POST",
  "path": "/",
  "headers": [ ["content-type","text/plain"], ["x-signature","..."], ... ],
  "body": "action: enter\nid: ...\ntrade_id: ...\n...",   // VERBATIM signed YAML
  "intent_id": "hs-us-2000-c1-enter",      // or null
  "trade_id": "hs-us-2000-c1",             // or null
  "status": 200,                            // the HTTP status you returned (0 = transport-error sentinel)
  "outcome": "entered",                     // your short outcome string
  "logs": [ { "level": "log", "msg": "entry id=..." }, ... ]   // every line you emitted, in order
}
```

This is exactly the struct in `src/recording.rs`. A bundle's `r2_records[]` is
**the chronological sequence of real requests** that hit you for this one trade
(typically: `prep` → `enter` → maybe re-fires → `close`), each paired with the
response you gave and the logs you emitted. **A record is a recorded
request→response→logs triple.**

## The replay model

For each `RequestRecord` in `inputs.r2_records` (in `ts` order):

1. **Reconstruct the request** from `body` + `headers` + `method` + `path`. The
   `body` is the verbatim signed YAML — feed it through the *same* entry path a
   live POST takes (`pub async fn main` / whatever request seam is cleanest to
   target in a native test). The signature header is already in `headers`, so
   signature verification should pass as-is against the recorded body.
2. **Mock the broker.** Your `core::broker::Broker` trait is already the seam —
   `run_action` / `run_enter` / `run_close` / `run_veto_with_broker` are all
   generic over `B: Broker`. Build a `MockBroker` that returns canned
   `place_order` / `get_open_trades` / `close_trade` results. Seed it from the
   bundle: `inputs.broker_fills` tells you what the broker *actually did* for
   this trade (the fill price, order id, position id, close price, realised
   P&L), so the mock can hand back matching order/position ids and prices,
   reproducing the broker side deterministically.
3. **Run the request** against your handler with the mock broker and the test
   state store you already use in `#[cfg(test)]` (the `StateStore` seam — KV is
   already abstracted for native tests).
4. **Assert** the replayed response matches the recorded one:
   - `status` matches `record.status` (treat the `0` sentinel as "transport
     error" — assert your replay also fails-soft, don't assert a real HTTP code).
   - `outcome` matches `record.outcome`.
   - `logs` reproduces `record.logs` — at minimum the **identity / decision
     lines** (`entry id=…`, `entry placed id=… order=…`, veto/rejection lines).
     Exact-match is ideal; if timestamps or volatile substrings leak in, assert
     on a normalised projection (the stable message prefixes), and say so.

Start with **`logs` and `outcome` equality** as the core assertion — those are
what the tax tracker's whole join is built on, so reproducing them proves the
replay is faithful. `status` is secondary.

## Gotchas — respect these, they will bite otherwise

- **`request_id` is your FNV hash, not Cloudflare's `$workers.requestId`.** It's
  `mint_request_id(body, headers)` — deterministic from the recorded body+headers.
  If your replay re-mints it, it will match (same input → same id). Do not key
  any dedup on it expecting it to equal a live `$workers.requestId`; the two
  schemes never collide.

- **Replay must NOT poison real KV / seen-index state.** Use the in-memory test
  `StateStore`. A faithful replay of an `enter` will call `mark_seen` on `Ok` —
  that's correct *within* the replay, but it means the **second** `enter` record
  in the same bundle (a legitimate multi-shot re-fire with a fresh `intent_id`,
  or a refused already-seen replay) depends on whether you reset the store
  between records. **Decide deliberately:** replay the whole record sequence
  against **one shared fresh store** so the seen-index / retry-gate / prep-state
  transitions are exercised end-to-end (this is the realistic and more valuable
  mode), rather than resetting per record. The recorded `status`/`outcome`
  sequence already encodes the right answer — e.g. a `409 already-seen` on a
  later record is the *expected* result and your replay should reproduce it.

- **`max_retries` / "retry" is multi-shot re-entry, not error-retry.** (Your
  CLAUDE.md already hammers this.) The bundle's record sequence is exactly the
  ground truth for multi-shot behaviour: place → fill → close → fresh signal →
  place again. The mock broker must return the recorded close (typically SL)
  between re-fires so the retry-gate sees the position close and allows the next
  placement.

- **`mark_seen` is written only on `ActionResult::Ok`** (the 2026-06 fix). A
  recorded `502` (broker placement failed) or a recorded veto/cooldown rejection
  should NOT have burned the seen index — so in replay, after a mock broker
  returns a placement error, the next fire of the same body must still be let
  through. The recorded sequence will demonstrate this; your assertion that the
  replay reproduces it is the regression guard for that exact fix.

- **Some records carry no parseable identity line** (a veto/expiry/parse-error).
  Their `logs[]` won't contain an `entry id=…`; the worker recovered intent
  fields from the verbatim `body`. For replay you still feed the `body` — the
  worker re-derives whatever it derived live. Just assert on `outcome`/`status`
  for those, since there may be no identity log line to match.

- **`status: 0` is your transport-error sentinel**, not a real HTTP status. When
  a record has `status: 0`, assert your replay also hits the fail-soft path,
  don't assert a numeric HTTP code.

- **`logs[]` order is emission order.** Assert in order; don't sort.

## Where to put it

- A `MockBroker` implementing `core::broker::Broker`, seeded from a
  `BrokerFill`-like spec (you can mirror the few fields you need rather than
  depending on the tax-tracker crate — keep this repo's dep graph clean).
- A bundle loader (serde structs mirroring the JSON above — you only strictly
  need `RequestRecord`, which you already have in `src/recording.rs`; derive
  `Deserialize` on it or a sibling, since today it's `Serialize`-only).
- A `replay_bundle(path)` test helper + a `#[test]` per checked-in fixture
  bundle under `tests/bundles/`.
- A couple of real fixture bundles (ask me to generate them with
  `trading-tax-tracker-cli bundle --trade-key <key> --r2-dir <dir> --out
  tests/bundles/<key>.json` for a known good trade and a known veto/refire trade).

## Deliverable

`cargo test` (native, off-wasm) loads each fixture bundle, replays its
`r2_records[]` against the mock broker through one shared test store, and asserts
the replayed `(status, outcome, logs)` sequence matches the recording. Green =
the worker's decision logic is pinned to real captured trades.

### One thing to confirm with me before building

Your `recording::RequestRecord` is currently `Serialize`-only. The cleanest
loader path is to add `#[derive(Deserialize)]` to it (or a `#[cfg(test)]` twin).
Tell me which you prefer and whether you want the fixture bundles generated now.
