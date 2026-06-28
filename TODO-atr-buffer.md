# TODO — ATR-based offset buffer + deprecate offset_pips

Make `offset_atr_pct` the primary signed-offset mechanism everywhere
`offset_pips` is used (H&S entry+SL, anchored SL/TP, CLI manual trades),
starting at 0.5% of ATR. Deprecate `offset_pips` but keep honoring it.

Report: `the-trading-academy/books/demo-journal/FEATURE-atr-based-entry-sl-buffer.md`

## Decisions (confirmed with user)

- **Resolution: fill-time (option b), shared in `trade_control_core`, pure+sync.**
  Buffer = `(offset_atr_pct / 100) × shell.atr`. `offset_atr_pct` is an
  **UNSIGNED magnitude**; the buffer's DIRECTION is derived from the anchor
  (`*High` anchors push up, `*Low` anchors push down — away from the candle,
  never toward it). This is cleaner than `offset_pips`' sign-in-the-value
  quirk and removes the sign-mistake bug class (see trade_patterns.rs comment).
  Negative `offset_atr_pct` is rejected at validation. `shell.atr` is already
  latched per-bar by the engine's signal state machine
  (`from_candle_and_signal` → `wilder_atr`) in BOTH worker (cron) and replay,
  recomputed from the candle window EACH tick (not stored with the arm), so no
  new ATR plumbing / no broker pull / no arm-timing race.
- **`shell.atr == None` only in ATR warmup** (`candles.len() <
  atr_length_for(gran)`) or a short/failed broker feed — NOT an arm-timing
  race (ATR is recomputed each tick, not stored with the arm). A golden H&S
  enter can't validly fire in warmup anyway. So `offset_atr_pct` set + `atr ==
  None` → **reject** (`ResolveError::AtrUnavailable`), fail-closed.
  IMPORTANT: reject = "skip THIS tick", NOT "discard the plan". The plan stays
  in `Phase::AwaitEntry` and the next golden bar re-evaluates (stateless
  retry — nothing parked that could TTL away / orphan, cf. Bug #15). Self-heals
  the moment ATR is computable. Log AtrUnavailable distinctly for observability.
- **`offset_atr_pct` XOR `offset_pips`** per ref/entry — both set is a reject.
- **Deprecate `offset_pips` everywhere, keep honoring it.** `#[deprecated]` +
  doc notes on the field across `PriceRef`/`EntrySpec`. Resolver still honors
  it for in-flight/old plans (no wire break, byte-identical serde for old
  intents). All NEW construction steers to `offset_atr_pct`.
- **CLI manual trades + anchored TP go ATR too.** CLI prompt offers ATR-pct
  (default) OR pips (for trades with no signal latch / where pips is wanted).
  Anchored TP gains `offset_atr_pct`. Default new H&S enter buffer = 0.5%.
- ATR period = existing per-timeframe `atr_length_for` (Wilder). Unchanged.

## Commit plan (each small, tested, green before next)

### Commit 1 — core: the field + resolution + errors (TESTS FIRST) ✅ DONE
- [x] Added `offset_atr_pct: Option<f64>` to `PriceRef::Anchored`,
      `EntrySpec::Stop`, `EntrySpec::Limit` (serde default + skip_if_none).
- [x] Deprecation doc notes on `offset_pips` (field stays, still honoured).
- [x] `OffsetError` enum (BothOffsetsSet / AtrPctOnCloseAnchor /
      NegativeAtrPct / AtrUnavailable) + `ResolveError::Offset` wrapper +
      `From<OffsetError>`.
- [x] Shared `resolve_offset(from, offset_pips, offset_atr_pct, shell,
      pip_size) -> Result<f64, OffsetError>` + `PriceAnchor::buffer_sign`
      (direction from anchor). Wired into `PriceRef::resolve` (now fallible)
      + the Stop/Limit arms + `resolve_tp`.
- [x] 7 new tests: buffer pushes away / scales with vol / atr-None rejects /
      both-set rejects / Close-anchor rejects / negative rejects / pips path
      unchanged. Engine `log_anchor` helper for the rejection logger.
- [x] Whole workspace builds + ALL tests pass (612 core / 59 engine / 217
      worker / 165 tv-arm …, 0 failed). clippy clean, fmt done.

### Commit 2 — engine/simulator parity
- [ ] Confirm simulator entry-level re-resolution honors the ATR buffer
      identically (it calls core resolution → should be automatic). Add a
      replay/simulator test pinning an ATR-buffered enter resolves the same
      in evaluate + simulate.

### Commit 3 — tv-arm / cli trade_patterns: H&S default to ATR 0.5%
- [ ] `DEFAULT_BUFFER_ATR_PCT = 0.5` const.
- [ ] H&S/iH&S enter bakes `offset_atr_pct: ±0.5` on entry + SL; stop emitting
      `offset_pips` for those. Sign per existing geometry table.
- [ ] Tests for the built enter intent.

### Commit 4 — CLI interactive + anchored TP
- [ ] `interactive.rs` prompts: choose ATR-pct (default) or pips for
      entry/SL/TP; emit the chosen field.
- [ ] script_validator: validate XOR + pct range (>0, sane upper bound).

### Commit 5 — docs
- [ ] README: new `offset_atr_pct` field, deprecation of `offset_pips`,
      new H&S default, fail-closed-on-warmup note.
- [ ] CHANGELOG vNN. Tag + push + advance parent pointer.

## Hazards
- Must land in BOTH replay + worker — both go through core
  `Resolved::from_intent`, so commit 1 covers both. (memory:
  strategy_changes_in_both_replayer_and_worker)
- M/W uses `MwParams` (spread_pips/pip_size mid→bid/ask), a SEPARATE pip
  mechanism — NOT `offset_pips`. Out of scope, leave untouched.
- entry_level_vetos (Bug #12) bake absolute prices — unaffected.
- Sign stays in the value (offset_atr_pct can be negative), for parity with
  offset_pips. Don't derive sign from anchor (would diverge from the mental
  model + the geometry table in trade_patterns.rs).
