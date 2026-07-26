# TODO — fixture corpus (SCOPING-fixture-corpus.md)

Each commit green (tests + clippy + fmt) before the next.

- [ ] **1. Shared `ReplayEconomics`** — extract private `Tally` from `report.rs`
      into a public outcome type; `report.rs` and `fixture.rs` both consume it.
      Fixes the report↔fixture divergence (§3.1). Retire the report-text
      workaround test `stateful_broker_books_reversal_and_expiry_closes_in_the_report`.
- [ ] **2. `outcome{}` in `expected.json`** (4.1) — `net_r`, counts, `legs[]`,
      sourced from `fire.realized`. Re-bless the 5 existing fixtures; any change
      to their `fires[]` means stop and read.
- [ ] **3. `arm{}` in `meta.json`** (4.2 + 4.5) — flags, versions, broker-qualified
      chart symbol, `journal_ref`. Wire `GIT_VERSION` into `replay-candles --version`.
- [ ] **4. Batch replay** (4.4) — `--test-mode --fixtures-glob` + `--json`.
      Also `--no-annotate` for unattended runs (after reproducing the collision).
- [ ] **5. `HsSpec` on `TradeSpec`** (2a) — `build_trade_plan` reads spec not
      `&Roles`. Pure refactor: byte-identical plans, proven by round-trip test.
- [ ] **6. `tv-arm --spec-in`** (2a) — arm from frozen spec, no TV. Re-read
      spread/mid/calendar per §3.5. Fix `--start` strict-RFC3339 here.
- [ ] **7. Tier-2 scored corpus** (2b) — aggregate Net R, baseline diff, re-bless.
      Not in `cargo test`.
- [ ] **8. `--save-matrix`** (4.3) — one-pass variants.

## Open questions

- `--annotate` collision (appendix): **reproduce before fixing**. `replay.rs:68-81`
  injects defaults before passthrough, clap is last-wins, and `replay.rs:177-196`
  tests the override works. The feature request says it fails.
