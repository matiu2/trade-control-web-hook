# TODO — Stage 5b: `contract_multiplier` baked onto the signed intent

Stage 5 of the IBKR futures integration (plan:
`~/.home-claude/plans/magical-enchanting-bird.md`). Stage 5 has two halves:

- **5a — `instrument-lookup` (DONE, shipped).** `Broker::Ibkr`,
  `AssetSymbols.ibkr`, and `Asset.futures: Option<FuturesSpec>` carrying the
  multiplier, plus `Asset::contract_multiplier()` returning `1.0` for spot.
  Merged as `01c3d41`, tagged `v4`, pushed. Rows: ES 50, MES 5, GC 100, MGC 10.
- **5b — this repo (this TODO).** Thread that number from the catalog, through
  `tv-arm`, onto the signed `Intent`.

Branch `feat/contract-calendar`, worktree `../trade-control-contract-calendar`.
Stages 2-4 are `9d43289` / `f9034ef` / `1516bb1`.

## Why the multiplier must be BAKED, not looked up

The worker has no instrument catalog linked. That is not an accident — baking
`pip_size` was tried the other way once, the worker looked it up, and it was
reverted. So the multiplier follows `pip_size` and `tick_size`: read from
`instrument-lookup` at arm time by `tv-arm`, written onto the intent, and
covered by the whole-body HMAC so it cannot be tampered in flight.

**Reading `1.0` for ES places a 50x oversized position.** That is the entire
reason this is one authoritative value rather than a constant per call site.

## Design decisions

- **Copy `tick_size`'s THREADING, not `pip_size`'s.** `pip_size` is duplicated
  into `MwParams`, a sync hazard CLAUDE.md itself warns about (`tv-arm` must set
  both copies and they can drift). `tick_size` is threaded as a plain argument.
  There is exactly one multiplier per intent; do not add a second copy anywhere.
- **But copy `pip_size`'s `validate()` arm.** `tick_size` has **no** validation,
  and that omission must not be inherited: a `0.0` multiplier divides sizing to
  infinity or zeroes the position, and `NaN` poisons every downstream number.
  New `IntentValidationError::ContractMultiplierInvalid`, mirroring
  `PipSizeInvalid`.
- **`Option<f64>` + `skip_serializing_if`.** A CFD intent must serialise
  **byte-identically** to a pre-feature one — that is the wire-compat anchor and
  it is testable.
- **Absent ⇒ `1.0`, at the consumer.** Same rule as the catalog side: a spot
  instrument is sized in units, so `1.0` is correct for it. Do NOT bake `1.0`
  onto every CFD intent — that would change every existing wire body for no gain
  and erase the distinction between "spot" and "futures with a missing value".
- **HMAC needs no change.** `core/src/sig.rs` line-scans the body with an
  *exclusion* allowlist, so a new `Intent` field is signed automatically. Assert
  it with a tamper test rather than assuming.

## ⚠️ Do NOT wire sizing in this stage

This stage carries the number as far as the intent and the dispatch seam. The
actual `contracts = budget / (stop_distance x multiplier x fx)` arithmetic is
**Stage 6b** (`broker-ibkr`'s private `risk.rs`), and integral/floor behaviour
plus `min_size`/`size_increment` belong there with the broker that reports them.
Nothing here may place an order — the staging rule is that nothing order-placing
lands before the close-out guard, and the guard is in, but the broker is not.

## Tasks

- [x] `Intent.contract_multiplier: Option<f64>` + doc comment.
- [x] `IntentValidationError::ContractMultiplierInvalid` + `Display` arm.
- [x] `validate()` arm rejecting non-finite / <= 0.
- [x] `tv-arm/src/precision.rs` — carry the multiplier alongside tick/pip.
- [x] `cli::TradeSpec` — carry it, and thread into the enter-alert builder.
- [x] The **third** baking site: `tv-arm/src/position_entry.rs`.
- [x] Read side: wherever sizing will consume it (dispatch/enter), plumbed but
      not yet arithmetic.
- [x] README: the multiplier's path and the "not looked up worker-side" rule.

## Tests (the correctness anchors)

- [x] A CFD intent's wire body is **byte-identical** to pre-feature (no
      `contract_multiplier:` key at all).
- [x] A futures intent round-trips the multiplier through sign -> parse.
- [x] **Signature tamper test**: editing the baked multiplier invalidates the
      HMAC (proves `sig.rs` covers the new field without being told to).
- [x] `validate()` rejects `0.0`, negative, and `NaN`.
- [x] Absent multiplier reads as `1.0` at the consumer.
- [x] An armed IBKR futures plan carries the catalog multiplier end-to-end.

## Mutation verification

Per `verify_new_analysis_code_by_mutation` — five applied, **all killed**:

- [x] `validate()` arm deleted ⇒ **4** tests, across two modules
      (`validate_rejects_{zero,negative,nan}_contract_multiplier` and
      `signed_path_rejects_a_zero_contract_multiplier`).
- [x] `skip_serializing_if` removed ⇒ `contract_multiplier_elided_when_none`.
      Asserted in **YAML**, the real wire encoding, which emits `null` for a
      bare `Option` — verified by this mutation. A TOML-based assertion would
      have passed regardless and proved nothing; that trap bit the sibling
      `instrument-lookup` change, see
      `[[toml_serialiser_omits_none_regardless_of_skip_if]]`.
- [x] `from_asset` reads `1.0` instead of the catalog multiplier ⇒ **3**
      precision tests.
- [x] The enter builder never bakes it ⇒ **2** end-to-end tests.
- [x] **`tick_size` and `contract_multiplier` transposed at the call site** ⇒
      the same 2 tests. This is the mutation `InstrumentSizing` exists to make
      catchable: as three adjacent positional `Option<f64>` parameters the swap
      would have compiled silently.

The HMAC needed no change, and that is **tested rather than assumed**:
`signed_path_contract_multiplier_tamper_rejected` edits the baked multiplier
after signing (50 → 1, the highest-leverage tamper available) and the signature
fails.

## Design notes worth keeping

- **`InstrumentSizing` groups pip + tick + multiplier.** `build_enter_alert`
  already took 30-odd arguments; three adjacent same-typed `Option<f64>`s are a
  transposition no type would catch, and transposing these three *is* the
  classic futures bug. The resolvers (`resolve_hs_trade` / `resolve_mw_trade`)
  likewise now take the whole `EffectivePrecision` instead of two loose `f64`s.
- **`EffectivePrecision::from_catalog`** replaced three hand-written struct
  literals on the TV-unavailable fallback paths. Futures *always* take a
  fallback path (they have no native instrument row), so a field silently
  dropped there would have been dropped for exactly the instruments that need
  it.
- **TradingView never overrides the multiplier**, though it does override tick.
  TV's `point_value` is a CFD per-point value for the chart's instrument, not
  the exchange contract size. Tested explicitly
  (`tradingview_never_overrides_the_multiplier`).
- **`DispatchConfig` gets no multiplier field.** It exists as an edge-resolved
  *fallback* tier for pip/tick; there is no sane fallback for a contract size,
  and inventing one would defeat the point of failing loud.

## Verified against the real binary

`trade-control build-trade` on a GC futures spec produced a full signed bundle:

- `05-enter.yaml` carries `contract_multiplier: 100.0` alongside
  `pip_size: 0.1` / `tick_size: 0.1` — three distinct numbers, unswapped.
- **No other alert** in the bundle carries the key (vetos/preps are never sized).
- The same spec rebuilt as EUR/USD emits **zero** occurrences of the key, so a
  CFD wire body is byte-identical to a pre-feature one.

## Gate

- **2889 workspace tests pass** (2874 at Stage 4), 0 failures.
- `cargo clippy --workspace --all-targets` — warnings confined to the same 5
  pre-existing files as Stage 4 (`engine/src/evaluate.rs`,
  `market-hours-gen/src/compute.rs`, `core/src/signals/state_machine.rs`,
  `cli/src/bin/replay_candles/spread_breakdown.rs`,
  `cli/examples/cache_range_tail_probe.rs`); none touched here.
- `cargo fmt --all --check` clean; no collapsed `\` continuations (scanned, then
  the binary was run).

## Note: `instrument-lookup` bumped 0.4 → 0.5

Stage 5a's API is needed here, so the six crates pinning `instrument-lookup`
(`cli`, `tv-arm`, `tv-news`, `journal`, `market-hours-gen`,
`spread-baseline-gen`) were bumped. That also surfaced two stale spots the new
`Broker::Ibkr` variant made visible in `tv-arm/src/instrument_resolution.rs`:
`to_il_broker` now resolves IBKR (its "the catalog has no futures broker column"
comment was written in Stage 4 and 5a added one), and `brokers_carrying` is
driven off `Broker::ALL` rather than a hand-written array.

## Status: COMPLETE

Stage 5 (both halves) is done. Next is **Stage 7** (`UnitsBelowMinimum` park),
which the plan sequences *before* Stage 6b, then Stage 6 (`broker-ibkr` +
integral sizing) where this multiplier is finally consumed by
`contracts = budget / (stop_distance × multiplier × fx)`.

⚠️ `cargo fmt` collapses `\` string continuations into literal spaces when the
reflowed line fits — it compiles and tests pass. Re-run any touched binary and
eyeball operator-facing messages (Stage 4 shipped two mangled ones this way).
