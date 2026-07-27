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
      `build_trade_plan`/`trigger_for` read plain data. Guarded by
      `a_plan_built_from_frozen_geometry_matches_one_built_from_drawings` — but
      "proven byte-identical" overstated it, and the test says so itself: its
      headline assertion reduces to `f(x) == f(x)` (both sides build from the same
      extracted geometry), so it cannot catch a field the extractor drops. That is
      exactly how `MwPath.runup_start` slipped through until 6a. The real coverage
      is the field-level assertions added alongside it
      (`a_fully_drawn_chart_freezes_every_geometry_field`, which asserts the
      key-set union of an H&S and an M/W chart against an explicit list).

      **A SECOND dropped field found 2026-07-27 (clean-slate review): `sr_levels`.**
      Same class as `runup_start` — drawn geometry no *trigger* reads, so it looked
      droppable — but `build_sr_ranges` widens each into a band and
      `!spec.sr_reversal_ranges.is_empty()` gates whether `07-close-on-sr-reversal`
      is armed at all. Worse than `runup_start` because it fails *quietly*: the
      default-on `tp_resistance_band` (derived from the fib, which IS in the
      geometry) keeps the vec non-empty, so a spec-in re-arm still emits the alert,
      just off the wrong levels. No error, no missing rule — only a different exit
      price, across 291 trades. Now in `PlanGeometry`; `resolve_hs_trade` and
      `build_trade_spec` take **no `Roles` at all**, so the seam is closed by the
      type system rather than by discipline. Two tests catch a regression (the
      key-set guard + `hs_drawn_sr_plus_auto_band`), both mutation-verified.

      Also closed: `check_required` had **zero** tests — replacing its body with
      `Ok(())` left all 293 green, and the `feaa019` one-point-neckline fix it
      contains was free to revert. Now one case per branch, each mutation-verified
      to redden ≥2 tests.
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

        **Prerequisite done (3b5e116).** The `&Roles` boundary is now closed by
        type: `register_trade_plan` and the whole M/W chain
        (`check_mw_required`, `resolve_mw_trade`, `resolve_mw_trade_with_spread`)
        take `&PlanGeometry`. All three were re-deriving
        `PlanGeometry::from_roles` internally, so extraction ran **three times**
        per arm off three borrows of the same drawings. **Only
        `run_position_entry` still takes `&Roles`** — correctly; position-tool
        SL/TP are TV drawing properties, so `--spec-in` must reject
        `--market-entry`/`--stop-entry`/`--limit-entry` anyway (already noted in
        6e).

        So the split point in `run` is now clean: **lines ~59–246 are chart +
        calendar read** (TvMcp, `get_state`, `get_range`, `list_drawings`,
        `classify`, the calendar fetch); everything after builds from `geom` +
        flags. `SetupInputs` is that first half's output.

        Found doing it: `MwPath` could not represent its own **anchor count** —
        a 5-anchor path truncated to four and armed as a different pattern than
        the one on screen, silently. Third instance of the dropped-geometry-field
        shape (after `runup_start` and `sr_levels`); see the pattern note in the
        `[[plan_geometry_dropped_field_pattern]]` memory.
  - [ ] **6e.** `--spec-in` itself: `FrozenSpec` file, guards, round-trip test.
        Reject `--market-entry`/`--stop-entry`/`--limit-entry` (position-tool SL/TP
        are TV drawing properties, inherently live-chart).
        **MUST carry `granularity`** — reviewer-flagged, verified. It feeds
        `TrendlineCross.bar_seconds` and trendline prices interpolate in
        *bar-index* space, so the same neckline read at H1 vs H4 gives different
        prices at the same instant. It currently comes from the LIVE chart
        (`resolution_to_granularity(state.resolution)`), so a re-arm off a chart
        left on another timeframe silently reprices the whole neckline — plausible
        numbers, wrong plan, no error. Deliberately not on `PlanGeometry` (a chart
        resolution isn't geometry); the frozen spec must carry it and either use it
        or refuse when the live chart disagrees. Noted in the `plan_geometry`
        module doc too.
  - [ ] **6f.** `--start` strict-RFC3339 fix (seconds currently mandatory).
- [x] **7. Tier-2 scored corpus** (2b) — DONE (6fae7f2) — `cli/src/bin/replay_candles/baseline.rs`.
      `--bless-baseline <file>` records a batch; `--baseline <file>` scores a later
      batch against it. Deliberately **does not affect the exit code** — a moved
      number is information, not a failure; `--check` stays the gate.

      Three design calls, each with a mutation-verified test:

      1. **Structural moves outrank magnitude moves.** A fixture flipping
         taken↔not-taken is a different event from one whose R shifted. Proven on
         the real corpus: a doctored baseline gave a **+0.15 aggregate
         improvement** that came entirely from gbpaud no longer taking a *loser*
         — surfaced first as `stopped`, not banked. Under |ΔR| sorting it sat
         below a −0.85 requantification.

         Hence `taken` reads the **legs**, not `net_r`. Three distinct things book
         0.0 R — declined, break-even, still-open-at-window-end — and only the
         first is "no trade". The tracked corpus already has one of each shape.
         Mutating `took_a_trade` to the tempting `net_r != 0.0` reddens its test.
      2. **No noise threshold.** Replay is deterministic, so a non-zero delta is
         never noise — it's always the code. A cutoff is a way of *not looking* at
         small moves, and small moves across 291 trades are how a regression
         hides. Sorted by magnitude so the operator can stop reading where they
         choose.
      3. **News-ON rows flagged advisory, not frozen** (the constraint recorded
         under "Decisions" below). `diff.reproducible()` is the set a regression
         hunt starts from; the render says up front when *no* mover is
         reproducible.

      Aggregates are summed in **BTreeMap key order on both sides**, so a
      bless-then-rediff reports exactly `+0.00`, not a last-bit residue. (This
      gate has lost two digits to summation order before — see the `account`
      decision below.) A hand-edited baseline is caught by comparing the stored
      total against a recomputed one.

      The diff prints **before** the failed-batch early return (a partial sweep is
      exactly when you want to see what moved) and labels itself INCOMPLETE.
      Blessing a partial batch warns: absent fixtures read as `added` on return.

      22 tests, 8 mutations tried and all 8 reddened a test. Verified end-to-end
      on the two tracked fixtures: bless → clean re-diff (exactly +0.00) →
      regression diff → hand-edit warning → `--json` stdout stays one parseable
      object (diff to stderr) → missing baseline exits 4.
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
  compare.

  **Correction:** an earlier version of this line said `net_r` is "a plain sum
  and bit-stable". That is wrong, and `economics.rs` says so at length. Float
  addition isn't associative either: summing the real uk-100 legs
  (`[0.549…, -1.0, 0.797…]`) over all 6 permutations yields **2 distinct bit
  patterns**. `net_r` is stable here only because leg order is deterministic
  (fire order) — not because summing is order-free. The difference from
  `account` is *degree*, not kind: one multiply-accumulate chain amplified the
  divergence enough to cross build profiles. Don't rely on "it's just a sum".
  (Checking this in Python will mislead you — the builtin `sum` is not a plain
  left fold.)
- **`fires[].fill` keeps its independent `simulate_fill` path.** The plan said
  commit 2 would move it onto `fire.realized`. On reading it properly, it earns
  its place as a second opinion on *bracket* physics (fill/sweep/never-trigger).
  It IS blind to reversal/expiry closes — documented, with `outcome` authoritative
  there.
  **OVERTURNED 2026-07-27 (`751efa8`) — this reasoning was wrong.** Measured on
  the uk-100 golden, `fires[].fill` was right on 1 of 5 rows: two were *phantom*
  (fabricated losing fills, with invented timestamps, for enters the report shows
  as `SUPERSEDED — resting order cancelled`), two were wrong (reversal-close read
  as `stopped_out` 0R vs really +0.549R; expiry-close read as unresolved
  `filled_open` vs really +0.797R). It was not a partial view — it answered in the
  same vocabulary as the right answer, so nothing flagged it. Deleted. A
  clean-slate reviewer independently reproduced the table and confirmed no
  regression coverage was lost (mutating the fire-bar skip in `find_fill` still
  reddens the gate via `outcome.legs`).

- **Untracked WIP fixtures are UNLOADABLE after this branch merges.** With
  `deny_unknown_fields` (`751efa8`), a stale `"fill"` key is a hard load error.
  In the primary checkout that is **four** fixtures: `coffee-sad` (4 keys),
  `eth-usd-missed`, `xau-xag-close-on-reversal`, `xau-xag-tp-resistance` (1 each).
  (`sgdjpy-spread-floor-min-r-block` has none and is fine.)

  **The remedy is a re-bless, not a strip.** All four also predate commit 2, so
  they carry no `outcome` block at all — stripping `fill` would make them load
  but leave them economically empty, which is useless for the grid and worse than
  a loud failure. Re-bless each from its own frozen inputs:

  ```sh
  replay-candles --test-mode --fixture <name> --rebless
  ```

  Cheap and offline (frozen candles, no broker). Deliberately NOT done for them
  here: re-blessing is destructive, and these are somebody's in-progress capture.
  Note the tracked goldens in the *primary checkout* also lack `outcome` — that's
  just this branch's own new field, not drift.

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

**~~Merge hazard:~~ RESOLVED (verified 2026-07-27).** This said the branch's
`cli/Cargo.toml` lacked `features = ["postgres-storage"]`. It doesn't — the
worktree, the primary checkout and `origin/main` all carry it on line 21. The
other agent's fix landed in `aeededb` and is already merged. No merge action.

---

## Session state (2026-07-27)

Branch `feat/fixture-corpus`, 21 commits, on `origin/main` @ `aeededb`.
All green: cli 277 lib + 200 bin + 22, tv-arm 293. Clippy clean on both crates,
`--check` green, debug and release agree to full float precision.

### Done
- **4.1–4.5 delivered** (the journalling side's whole request).
- **PlanGeometry** (commit 5) + **6a** (`MwPath.runup_start`) + **6b** (validation
  /TP/entry-vetos read geometry) + **6c** (`ControlWindows`, generic bundle loop).
- **Pre-existing bugs fixed**: the `XDG_CONFIG_HOME` test race (0/30, was ~5-10%);
  a unit test reading the operator's real credential store; an empty
  `XDG_CONFIG_HOME` writing expiry anchors to `/trade-control/expiry`.

### Reviewed — three rounds
Two adversarial reviews on disjoint file sets, then a **clean-slate** review with
no knowledge of the reasoning behind the code. The third round was the most
valuable: everything it found looked deliberate from the inside.

- **tv-arm refactor: clean.** Differential-tested old-vs-new exhaustively, zero
  divergences; mutation-tested the harness first. Reviewer noted the changelog
  mentions only ONE of two guard tightenings — the 1-point *fib* tightening
  (Fatal → Reject) is undocumented. Still worth a line.
- **economics/batch: three real bugs**, fixed in 18af721.
- **Clean-slate round: 15 findings, 12 actioned** (41a17a2, 2d73632, 75363ec,
  5b76cd7, dabf17b, 54ffe67). The three it called blockers were real:
  - `--json` emitted **zero bytes** on the live path (now `requires` test-mode).
  - `--rebless --simulate false` silently deleted a golden's economics (now
    exit 4).
  - **13 fixture tests reported `ok` in 0.00s with the corpus moved aside** — a
    gate that goes green when its evidence vanishes. Now 4 loud failures.

  Plus: a `--check` mismatch discarded the number it measured (now keeps
  `outcome` + `expected_net_r`, so a red sweep is diagnosable); non-finite
  prices could write an **unloadable** golden (`serde_json` writes `NaN` as
  `null`); a zero-match glob exited 0; three tests provably could not fail.

  **Two findings I disputed, with reasons** — a "clamp implausible R" suggestion
  (declined: clamping hides the data glitch; it warns instead) and one
  misattribution (the per-broker cache change is the other agent's, already on
  `origin/main` in `aeededb`, and is documented).

  Not actioned, deliberately: `--fixture` path traversal (single-user local
  tool), and `held_realized_outcome` writing `stop_loss = entry_price` for an
  open position (pre-existing, 0R either way — but the new economics module now
  trusts it as input, so worth a look before tier-2 scoring).

### Next (unstarted)
- **6d** extract `SetupInputs` + `arm_from_inputs`; `run` becomes chart-vs-frozen.
      This is also where `pipeline.rs` finally gets under control — 6c left its
      non-test body at 2269 lines (it only shrank by 6; the generic bundle
      machinery gave back what the module extraction took out).
- **6e** `--spec-in` + `FrozenSpec` + round-trip test.
- **6f** `--start` strict-RFC3339 fix.
- **8** `--save-matrix`. Independent of 6.

### The corpus is now usable end-to-end

With 7 done, generating and scoring the 291-trade grid needs no further tooling:

```sh
# one-time, per trade: capture the six variants (live arm, needs TV + broker)
tv-arm-staging … --save-fixture trade-124-normal-news-on
# … × 3 entry rules × news on/off

# offline from here — no TV, no broker, no network, parallel-safe
replay-candles --test-mode --fixtures-glob '*' --bless-baseline corpus-v113.json \
               --baseline-label v113

# after an engine change
replay-candles --test-mode --fixtures-glob '*' --baseline corpus-v113.json
```

**8 (`--save-matrix`) is now a convenience, not a blocker** — it collapses the
six capture runs into one pass. Worth doing at 291 trades; not on the critical
path to an answer.

### Open items for the operator
- `broker-tradenation-v0.14.0` is pushed as a BRANCH (`feat/testable-account-store`)
  for review, not merged. All seven Cargo.tomls here already point at the tag.
  PR: https://github.com/matiu2/tradenation-api/pull/new/feat/testable-account-store
- Backup at `~/.config/tradenation/accounts.enc.bak-before-test-fix` — re-verified
  **byte-identical** to the live store, so nothing was corrupted. Safe to delete;
  left in place because it's a credential file and that's your call.
- `annotate.rs` still has two `unsafe set_var("HOME")`. Deliberately left: that
  one is honest (correct comment, lock genuinely covers both mutators). Separate
  cleanup, not a bug.
- ~~Reviewer flagged `GIT_VERSION` staleness~~ — **NOT REPRODUCIBLE, closed
  2026-07-27.** The claim was that `cli/build.rs` only reruns on
  `.git/HEAD`/`refs/tags`, so `engine_version` could lag the code. It can't: any
  source change recompiles the crate, which reruns `build.rs` and re-reads `git
  describe`. Verified twice, independently — appended a comment to
  `economics.rs`, rebuilt, `--version` went `v116-62-g751efa8` →
  `…-g751efa8-dirty`; removed it and the `-dirty` tracked the remaining real
  edits. The rerun-if directives are a *lower* bound on when it reruns, not a
  ceiling. No action.
