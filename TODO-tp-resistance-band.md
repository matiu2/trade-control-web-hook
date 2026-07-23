# Auto TP-resistance band on every H&S trade (tv-arm)

Put a one-sided S/R band pinned to the take-profit onto every H&S / iH&S trade,
default-on, so a golden reversal near TP closes the position for a partial win
instead of round-tripping to the stop. Feeds the existing
`07-close-on-sr-reversal` machinery via `TradeSpec.sr_reversal_ranges`.

## Tasks

- [x] `args.rs`: add `--tp-resistance-pct` (default 0.1) and `--skip-tp-resistance`
- [x] `args.rs`: tests (defaults + flag parsing)
- [x] `pipeline.rs`: `tp_resistance_band(tp, direction, pct)` helper (far edge = TP)
- [x] `pipeline.rs`: append auto band into `sr_reversal_ranges` in `build_trade_spec`
      (H&S only; M/W path left as `Vec::new()`)
- [x] `pipeline.rs`: tests (band geometry long/short; default adds band; skip flag
      empties it; drawn S/R + auto = 2 bands)
- [x] `cargo test -p tv-arm` green (259 pass)
- [x] `cargo test -p trade-control-cli` green (existing sr-reversal-close tests)
- [x] `cargo clippy -p tv-arm` clean
- [x] `cargo fmt`
- [x] README: document `--tp-resistance-pct` / `--skip-tp-resistance`
- [x] e2e: build-trade --from-file with sr_reversal_ranges → valid signed
      07-close-on-sr-reversal (band far edge = TP, inside_window:[price])
- [ ] commit + push; advance parent submodule pointer
