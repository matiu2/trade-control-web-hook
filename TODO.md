# TODO — `--bcr-require-golden`  ✅ DONE

Require the break-and-close (03) and retest (04) candles to be **golden**
(bar range `h - l` ≥ ATR at the crossing bar), as an opt-in gate. Off by
default → byte-identical to current behaviour.

**NB:** this is a *new* engine check, not a reuse of `needs_golden`.
Metric chosen: **full range `h - l` ≥ ATR**. One flag gates **both** break
and retest.

## Steps

- [x] `engine`: `TradePlan.bcr_require_golden: bool` (`#[serde(default)]`), signed.
- [x] `engine`: `bar_is_golden(candle, window, gran)` — fail-closed + warn! on
      ATR-unavailable. Tests: compares-range-to-ATR, fails-closed-short-window.
- [x] `engine`: gate `stamp_break`. Test: `bcr_require_golden_gates_the_break_and_close`.
- [x] `engine`: gate `stamp_retest`. Test: `bcr_require_golden_gates_the_retest`.
- [x] Replay uses the same `evaluate` path → replay == live for free.
- [x] `tv-arm`: `--bcr-require-golden` arg + thread through
      `register_trade_plan`/`build_trade_plan`. Test: flag bakes onto plan JSON.
- [x] `core`: round-trip + default-false tests.
- [x] README (03 + 04 rows) + CHANGELOG v108.
- [x] clippy + fmt green; core/engine/tv-arm tests pass.

## Ship

- [x] commit + push branch
- [x] merge to staging + main
- [ ] advance parent submodule pointer, tag v108
- [ ] deploy dev + staging
- [ ] memory note
