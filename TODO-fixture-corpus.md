# TODO — fixture corpus (SCOPING-fixture-corpus.md)

Each commit green (tests + clippy + fmt) before the next.

- [x] **1. Shared `ReplayEconomics`** — DONE (4df52f4) — extract private `Tally` from `report.rs`
      into a public outcome type; `report.rs` and `fixture.rs` both consume it.
      Fixes the report↔fixture divergence (§3.1). Retire the report-text
      workaround test `stateful_broker_books_reversal_and_expiry_closes_in_the_report`.
- [x] **2. `outcome{}` in `expected.json`** (4.1) — DONE (164423a) — `net_r`, counts, `legs[]`,
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

## Decisions made along the way

- **`account` is derived, not stored.** Storing the compounded balance made the
  fixture gate flaky — it's a multiply-accumulate chain whose low bits depend on
  FP operation order, so the test build and release build disagreed in the last
  two digits (`...607` vs `...609`) and `ReplayOutcome` equality is exact float
  compare. `net_r` is a plain sum and bit-stable.
- **`fires[].fill` keeps its independent `simulate_fill` path.** The plan said
  commit 2 would move it onto `fire.realized`. On reading it properly, it earns
  its place as a second opinion on *bracket* physics (fill/sweep/never-trigger).
  It IS blind to reversal/expiry closes — documented, with `outcome` authoritative
  there.
- **Only 2 of the 5 local fixtures are git-tracked.** `sgdjpy-spread-floor-min-r-block`,
  `xau-xag-close-on-reversal`, `xau-xag-tp-resistance` are untracked WIP in the
  primary checkout and were deliberately left alone (not re-blessed).

## Open questions

- `--annotate` collision (appendix): **reproduce before fixing**. `replay.rs:68-81`
  injects defaults before passthrough, clap is last-wins, and `replay.rs:177-196`
  tests the override works. The feature request says it fails.

## Interaction with DEV-BRIEF-postgres-candle-cache.md (3rd agent, in flight)

That brief fixes a real data-integrity bug: `cli/Cargo.toml` never opted into
`candle-cache`'s `postgres-storage`, so it fell back to ReDB's **exclusive file
lock** — two concurrent replays and one dies. Worse, a crashed run and a
legitimate no-fill run both produce no `Net R:` line, so a driver can't tell
them apart. Two concurrent batches each silently lost a different random subset
of cells; both grids looked complete.

**The fixture corpus is immune to that bug, by construction.** `run_test_mode`
makes **no candle-cache calls at all** — frozen candles come off disk. Verified
empirically on the *unfixed* ReDB build: 6 concurrent `--test-mode` replays of
the same fixture all returned `Net R: +0.35`, none dropped. The lock contention
exists only on the **live-arm** path, which is exactly the path the corpus is
designed to run once per trade and never again.

So their verification command (two concurrent `tv-arm … replay`) genuinely needs
their fix; `replay-candles --test-mode --fixtures-glob` (commit 4 here) is
parallel-safe already.

**Shared surface — coordinate:**
- Their Part-2 ask #5 (`--json` result mode) IS commit 4 here. Their brief notes
  the overlap. Commit 4's `--json` must therefore ALWAYS emit an object — a
  failed run carries an error field and a null outcome — which satisfies their
  ask #2 (never let absence-of-line mean two different things) structurally
  rather than by patching stdout text.
- Their exit-code + loud-infra-error asks (#1, #3, #4) and the Cargo feature
  switch are **theirs**; don't touch `cli/Cargo.toml` from this branch — this
  worktree branched before their fix landed and would revert it on merge.
- `--annotate` collision and `--start` strict-RFC3339 appear in BOTH documents.
  Two independent reports raises confidence the `--annotate` one is real despite
  `replay.rs:177-196` asserting the override works. Reproduce before fixing.

**Merge hazard:** this branch's `cli/Cargo.toml` still lacks
`features = ["postgres-storage"]` (their uncommitted fix is in the primary
checkout). Take THEIR version of that line when merging.
