# TODO — fix `06-close-on-reversal` never evaluated (engine + replay)

Bug: `BUG-replay-close-on-reversal-not-evaluated.md`. A `06-close-on-reversal`
rule (`Trigger::PinePattern`, `action: close`, `dir` = opposite of trade) never
fires — neither in the worker nor in replay — because `eval_trigger` returns
`false` for every `PinePattern` trigger (only `evaluate_entry`'s `eval_pine_entry`
ever runs pine detection). So an open position is over-held to SL/TP/window-end.

This is a SHARED-engine gap (see strategy_changes_in_both_replayer_and_worker):
fixing the engine fixes the worker dispatch AND the replay fire.

## Plan

- [x] **Engine — fire a `PinePattern` guard via the detector.** In
      `evaluate_guards`, when a guard rule's trigger is `PinePattern`, route it
      through `eval_pine_entry` (same detector as the enter), gated by direction.
      On a detector fire, also require the reversal candle's price to sit inside
      one of the intent's `sr_bands` (when `inside_window` lists `price`) — the
      pure half of the worker's `run_close` contextual gate, so the engine only
      fires a real reversal-close. News-window gate stays the worker's job (KV).
      Push the intent **with the latched signal shell** (so `run_close` sees
      golden/confirmed) and set `Phase::Done`.
  - [x] Add a pure `price_in_any_band(price, bands)` helper to the engine
        (mirrors `src/lib.rs::price_band_hit`; worker copy stays — it reads a
        live broker price, not a candle).
  - [x] Tests: a short plan + a long reversal candle in an SR band fires the
        close guard; the same candle OUTSIDE every band does not; a long
        reversal with no band requirement still fires; same-direction ignored.

- [x] **Replay — exit the open position on a close fire.** The fill simulator
      (`report.rs`) walks each enter's forward path independently and ignored
      close fires. A `close` fire that lands after an enter's fill and before its
      SL/TP now flattens the position at the close bar's close price.
  - [x] Post-pass `apply_reversal_close` threads close fires into the per-enter
        fill resolution (report + annotate share `resolve_fire`, now `+closes`).
  - [x] Surface it: `fill: CLOSED ON REVERSAL — in @ … → exit … (<bar>)`,
        tallied under `REV:` distinct from TP/SL; annotate label `reversal`.
  - [x] Test: end-to-end `run`→`render` (multi-shot short fills, bullish reversal
        in band closes it) + `apply_reversal_close` unit matrix.

- [x] cargo clippy (-D warnings) + fmt + full workspace test green
      (core 605, engine 67, worker 217, trading_view 34, tv-arm 165, tv-news 76,
      cli replay 48).
- [x] README (engine close-on-reversal note + Candle replay note) + CHANGELOG.
- [ ] Commit + push branch. Tag/advance parent + deploy: defer to user —
      staging is mid-bake (v60 marker), so DON'T disturb it; this is a
      main/dev-targeted fix.

## Verification

Re-replay trade 075 Wheat: leg 3 closes on 06-25 23:00 ≈ 5.860 (+~0.16R), not
held to window-end. `pine-close` evaluations > 0 in the debug log.
