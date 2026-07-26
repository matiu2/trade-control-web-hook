# TODO — fixture corpus (SCOPING-fixture-corpus.md)

Each commit green (tests + clippy + fmt) before the next.

- [x] **1. Shared `ReplayEconomics`** — DONE (4df52f4) — extract private `Tally` from `report.rs`
      into a public outcome type; `report.rs` and `fixture.rs` both consume it.
      Fixes the report↔fixture divergence (§3.1). Retire the report-text
      workaround test `stateful_broker_books_reversal_and_expiry_closes_in_the_report`.
- [x] **2. `outcome{}` in `expected.json`** (4.1) — DONE (164423a) — `net_r`, counts, `legs[]`,
      sourced from `fire.realized`. Re-bless the 5 existing fixtures; any change
      to their `fires[]` means stop and read.
- [x] **3. `arm{}` in `meta.json`** (4.2 + 4.5) — DONE (e9704fb) — flags, versions, broker-qualified
      chart symbol, `journal_ref`. Wire `GIT_VERSION` into `replay-candles --version`.
- [x] **4. Batch replay** (4.4) — DONE (b2502d5) — `--test-mode --fixtures-glob` + `--json`.
      Also `--no-annotate` for unattended runs (after reproducing the collision).
- [x] **5. `PlanGeometry`** (2a) — DONE (3bfcab1). Landed as `tv-arm/src/plan_geometry.rs`
      rather than `HsSpec` on `TradeSpec` (see the decision note below).
      `build_trade_plan`/`trigger_for` read plain data; proven byte-identical by
      `a_plan_built_from_frozen_geometry_matches_one_built_from_drawings`.
- [ ] **6. `tv-arm --spec-in`** (2a) — arm from frozen spec, no TV. Bigger than
      one commit; sequenced:
  - [x] **6a.** `MwPath.runup_start` (4713acb) — latent bug: direction + 2 gates
        need it, no trigger does.
  - [x] **6b.** Push `PlanGeometry` through validation / TP / entry-level vetos
        (feaa019). Found + fixed a 1-point-neckline hole in `check_required`.
  - [x] **6c.** `ControlWindows` owns the three calendar-derived fields + the
        prune (60306b1); `Roles` is immutable after `classify`. Collapsed the two
        near-identical bundle builders into one generic `build_all::<K>`. Found
        two untested behaviours: `close_on_news` had **no** test at all, and
        nothing asserted pause/news bundles land in disjoint dirs (both write
        `manifest.yaml`, so a collision silently clobbers one).
  - [ ] **6d.** Extract `SetupInputs` + `arm_from_inputs`; `run` becomes a two-way
        branch (chart vs frozen). `mcp: Option<TvMcp>` makes "annotation is
        chart-only" structural.
  - [ ] **6e.** `--spec-in` itself: `FrozenSpec` file, guards, round-trip test.
        Reject `--market-entry`/`--stop-entry`/`--limit-entry` (position-tool SL/TP
        are TV drawing properties, inherently live-chart).
  - [ ] **6f.** `--start` strict-RFC3339 fix (seconds currently mandatory).
- [ ] **7. Tier-2 scored corpus** (2b) — aggregate Net R, baseline diff, re-bless.
      Not in `cargo test`.
- [ ] **8. `--save-matrix`** (4.3) — one-pass variants.

## Decisions made along the way

- **`PlanGeometry` in tv-arm, NOT `HsSpec` on `TradeSpec`.** The scoping doc
  proposed hanging the geometry off `cli::TradeSpec`. On reading the code,
  `TradeSpec` is what arming *produces and signs* — putting frozen input geometry
  on it would ride the geometry into every signed alert body with no consumer.
  `PlanGeometry` is tv-arm's *input*; `TradeSpec` stays its output. One artifact
  per job.
- **News is re-read, so a spec-in arm is NOT bit-reproducible across time.**
  `close_on_news` derives from the re-read calendar, so news-ON cells can move for
  calendar reasons rather than engine reasons. Correct by design (you want fresh
  news), but the tier-2 baseline diff must label news-sensitive rows so that
  movement isn't mistaken for a regression. News-OFF rows stay reproducible.

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

**All of the journalling side's asks (4.1–4.5) are DELIVERED.** 5–8 below are the
operator-requested expansion (re-armable spec + scored corpus).

## Resolved questions

- `--annotate` collision: **REAL, and fixed** (e9704fb). Both reports were right.
  `ArgAction::Set` REJECTS a repeated flag — it is not last-wins. The existing test
  passed because it asserted only on TOKEN POSITION and never parsed the argv; its
  comment ("clap: last wins") was simply false. Lesson: when testing argv
  assembly, parse the result.

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

---

## Session state (2026-07-26, before compact)

Branch `feat/fixture-corpus`, 14 commits, rebased onto `origin/main` @ `aeededb`.
All green: 772 tests (cli 274 lib + 193 bin + 22; tv-arm 283), clippy clean,
fixture goldens unchanged, debug and release agree to full float precision.

### Done
- **4.1–4.5 delivered** (the journalling side's whole request).
- **PlanGeometry** (commit 5) + **6a** (`MwPath.runup_start`) + **6b** (validation
  /TP/entry-vetos read geometry).
- **Two pre-existing bugs fixed**: the `XDG_CONFIG_HOME` test race (0/30 now, was
  ~5-10%) and a unit test reading the operator's real credential store.

### Reviewed
Two adversarial reviews run on disjoint file sets.
- **tv-arm refactor: clean.** Differential-tested old-vs-new exhaustively, zero
  divergences; mutation-tested the harness first to prove it catches injected
  bugs. Two guard tightenings, both proven to only reject setups that could never
  have entered. Reviewer noted the changelog mentions only ONE of them — the
  1-point *fib* tightening (Fatal → Reject) is undocumented. Worth a line.
- **economics/batch: three real bugs, all fixed** in 18af721.

### Next (unstarted)
- **6d** extract `SetupInputs` + `arm_from_inputs`; `run` becomes chart-vs-frozen.
      This is also where `pipeline.rs` finally gets under control — 6c left its
      non-test body at 2269 lines (it only shrank by 6; the generic bundle
      machinery gave back what the module extraction took out).
- **6e** `--spec-in` + `FrozenSpec` + round-trip test.
- **6f** `--start` strict-RFC3339 fix.
- **7** tier-2 scored corpus (aggregate Net R + baseline diff). Independent of 6.
- **8** `--save-matrix`. Independent of 6.

### Open items for the operator
- `broker-tradenation-v0.14.0` is pushed as a BRANCH (`feat/testable-account-store`)
  for review, not merged. All seven Cargo.tomls here already point at the tag.
  PR: https://github.com/matiu2/tradenation-api/pull/new/feat/testable-account-store
- Backup at `~/.config/tradenation/accounts.enc.bak-before-test-fix` — safe to
  delete once satisfied.
- `annotate.rs` still has two `unsafe set_var("HOME")`. Deliberately left: that
  one is honest (correct comment, lock genuinely covers both mutators). Separate
  cleanup, not a bug.
- Reviewer flagged `GIT_VERSION` staleness: `cli/build.rs` only reruns on
  `.git/HEAD`/`refs/tags`, so `engine_version` can lag the code that produced an
  outcome. Matters if it gates a blessed baseline.
