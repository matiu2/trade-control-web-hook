# TODO — `sl` chart Note as a fixed stop-loss

Operator drops a TradingView **Note** on the chart saying `sl` (or
`stop-loss`), anchored at the price they want the stop — typically the
shoulder or the head. `tv-arm-staging` reads it and uses that price as the
stop instead of the geometry-anchored default, plus a volatility buffer
resolved at fire time.

## Decisions (settled with the operator before coding)

| question | answer | why |
|---|---|---|
| which drawing | TradingView **Note** → tv-mcp kind `text_note` | confirmed empirically against the live GBP/NZD chart |
| which anchor | `points[0].price` | `points[1]` is the box's other corner and drifts; `start_note.rs` already reads `points[0].time` for `--start` |
| scope | **H&S path only** (`hs_resolve.rs`) | M/W keeps worker-computed geometry |
| buffer | **ATR, resolved at fire time** (new `PriceRef` variant) | see "Why not bake the buffer" below |
| buffer sign | from **fib direction** (`direction_from_head_neckline`) | short → SL above (+1); long → SL below (−1) |
| which note counts | anchored in `[fib_earliest − 5 bars, trade_expiry]` | a stale note from an older setup must not win |
| no note | today's geometry-anchored SL, byte-identical | the feature is opt-in |
| wrong-side note | **hard error** at arm time | a stale note must never silently invert the stop |
| 2+ notes in window | **hard error** | ambiguous, same contract as the `start` note |

### Why not bake the buffer at arm time

The first instinct — have tv-arm compute `drawn + sign × atr_pct × ATR` and
emit a plain `PriceRef::Absolute` — is **wrong** and must not be reinstated:

- `tv-arm/src/plan_geometry.rs` (module docs) forbids freezing time-varying
  values into a plan, and names ATR explicitly: *"ATR isn't here either: the
  engine computes it from candles, so there's nothing to carry."*
- tv-arm has **no candle feed** (no `candle-cache` / `atr` dependency), so it
  has no ATR to bake even if we wanted one.
- A baked buffer would break `--spec-in` re-arms, which exist precisely to
  rebuild a plan with *today's* logic against *today's* volatility.

The **sign** is fine to bake — direction is a property of the setup, not of
the moment. Only the magnitude is time-varying, so only the magnitude is
deferred to fire time.

## The wire change (the risky bit)

`PriceRef` is `#[serde(untagged)]` **and HMAC-signed**. Two hazards:

1. **Declaration order matters.** Untagged serde tries variants top-down and
   takes the first that parses. `AbsoluteBuffered` MUST be declared **before**
   `Absolute`, or `{absolute, offset_atr_pct, sign}` parses as bare `Absolute`
   and the buffer is **silently dropped** — a stop at the wrong price, no error.
   Test `absolute_buffered_does_not_parse_as_absolute` pins this.
2. **Old plans must stay byte-identical.** A plain `{absolute: x}` must still
   round-trip as `Absolute`, and an `Anchored` SL must be untouched, or every
   in-flight signed plan breaks.

Resolution funnels through **one** place — `PriceRef::resolve`
(`core/src/intent.rs`) — reached by both the worker (`resolution.rs:267`) and
the replay engine (`engine/src/evaluate.rs:1448`). So worker/replay parity is
structural, not something to maintain by hand. Do **not** add a second
resolution path.

## Commits

- [ ] **1 — core: `PriceRef::AbsoluteBuffered`**
  - [ ] variant declared **above** `Absolute`; `{absolute, offset_atr_pct, sign}`
  - [ ] `PriceRef::resolve` arm: `absolute + sign × (pct/100) × shell.atr`
  - [ ] reuse `OffsetError::AtrUnavailable` / `NegativeAtrPct` — no new error type,
        no silent fallback when ATR is missing (loud failure, per house rule)
  - [ ] tests: resolves both directions; ATR-missing rejects; negative pct rejects;
        **untagged-order test**; old `Absolute` / `Anchored` round-trip unchanged
- [ ] **2 — conventions: `SL_LABELS`**
  - [ ] `&["sl", "stop-loss"]` in `conventions/src/labels.rs`
  - [ ] NOTE: `matches()` is exact-match; real notes are multi-line
        (`"v2 entry\ncontinuation"` seen on the live chart). Decide + test:
        exact-match only, or first-line match. Exact keeps it unambiguous.
- [ ] **3 — tv-arm: read the note**
  - [ ] new `tv-arm/src/sl_note.rs` (own module, one idea — mirrors `start_note.rs`)
  - [ ] filter stubs to `text_note`, match label, bound by
        `[fib_earliest − 5×bar_seconds, trade_expiry]`
  - [ ] 0 → `None`; 2+ → error; 1 → `points[0].price`
  - [ ] tests: in/out of window, duplicate, missing anchor, degenerate point
- [ ] **4 — tv-arm: wire into the H&S spec**
  - [ ] `Roles.sl_note` + `PlanGeometry.stop_loss` (frozen — a drawn price is
        setup geometry, so it MUST be in `PlanGeometry` or `--spec-in` re-arms
        lose it; see the dropped-field pattern that has bitten 3× already)
  - [ ] wrong-side check against direction → hard error
  - [ ] `TradeSpec.sl_price` + new `sl_buffer_atr_pct`, both → `AbsoluteBuffered`
  - [ ] tests: short/long correct side, wrong side rejected, absent = unchanged
- [ ] **5 — docs + fixture**
  - [ ] README (wire format + operator workflow), CLAUDE.md hazards section
  - [ ] CHANGELOG entry, `vNN` tag
  - [ ] replay fixture proving the buffered SL resolves in replay too

## Verification

Green tests prove little here — per the house rule, verify by **mutation**:
flip the buffer sign, swap the variant declaration order, delete the
wrong-side check; each must turn a test red. If it doesn't, the test is
scaffolding, not behaviour.

Then: `cargo clippy` + `cargo fmt` per crate, dry-run on demo before live.
