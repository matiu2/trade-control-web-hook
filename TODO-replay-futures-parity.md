# TODO — Stage 8: replay parity for futures

Stage 8 of the IBKR futures integration (plan:
`~/.home-claude/plans/magical-enchanting-bird.md`). Branch
`feat/contract-calendar`, worktree `../trade-control-contract-calendar`.

Stages 2-7 are done and pushed (`9d43289`, `f9034ef`, `1516bb1`, `0ae38c0`,
`e457e2c`, `65d9e0e`, `cb294c2`).

## What this stage does — and what it deliberately does NOT

The scoping doc wanted contract multipliers and integral sizing inside
`ReplayBroker`. **Decided against, operator-confirmed** (plan, "Replay parity"):
sizing needs live equity + an FX rate, which an offline replay by definition has
not got. `ReplayBroker` reports `size: None` *by documented design*
(`replay_broker.rs::replay_placement`), and replay economics are pure
R-multiples off a synthetic `START_ACCOUNT`. Inventing an equity model there
would make the numbers *less* honest, not more.

So the gap is **accepted with three mitigations** — A closes the closeable half,
B names the bias where a reader will see it, C measures the accepted gap instead
of simulating it.

### A — the close-out guard, offline

The close-out deadline is pure calendar arithmetic over the baked table. It has
no dependency on equity, FX or a broker, so it is **fully deterministic
offline** and belongs in replay. The scoping doc bundled it with the impossible
sizing half, which is the only reason it looked blocked.

### B — name the bias in the report

One line, same idiom as `IMPLAUSIBLE_R`: *"sizing not simulated — live may
reject entries that floor to 0 contracts."* A reader comparing a replay's net R
against a live account must be told which direction the difference runs.

### C — an offline granularity probe

Given a **stated** hypothetical account size, count how many fills would have
floored to 0 contracts. A **coverage statistic, not a simulation** — it consumes
a number the operator supplies rather than inventing an equity model, which is
what keeps it honest. It directly answers the promotion-ladder question ("is a
$10k account big enough to trade MGC?").

## Tasks

- [x] A. Close-out guard reachable from the replay path.
- [x] B. Report warning naming the sizing gap.
- [x] C. `--probe-account <amount>` granularity probe.
- [x] Record the decision beside `PARITY.md` / `REPLAY-PARITY-AUDIT.md`.
- [x] README section (and corrected Stage 4's now-stale refusal table).

Not written twice: the close-out decision lives in ONE function
(`cli/src/close_out_check.rs::verdict`), called by both the arm-time guard and
the replay. The arm-time guard was refactored onto it with no behaviour change
(all 328 cli lib tests unchanged), so the two cannot drift.

## Tests (the correctness anchors)

- [x] A replay of a futures plan past its close-out is flagged.
- [x] The same plan before its close-out is not.
- [x] A CFD replay is completely untouched (**910/910 golden fixtures pass,
      Net R +294.32 unchanged** — the scope proof).
- [x] Long and short differ on the same contract and day (the month-early trap
      reaching the replay).
- [x] The probe counts a floor-to-0 fill at a small account and none at a
      large one, on the same legs.
- [x] A micro contract is placeable where the full size is not.
- [x] The probe refuses (not silently 0) when the multiplier is unknown, or
      unusable (0 / NaN / negative).
- [x] The arm-by day itself is still armable; the day after is not.

Dropped from the plan: *"the warning appears only when the run placed entries"*.
The sizing caveat must print for **every** futures replay — a run that placed
nothing is exactly when a reader might conclude the strategy is flat rather
than that sizing was never modelled.

## Mutation verification

Per `verify_new_analysis_code_by_mutation` — **9 applied, 9 killed**:

| # | Mutation | Killed by |
|---|---|---|
| 1 | probe rounds instead of flooring | 3 tests |
| 2 | unknown multiplier defaults to `1.0` | 2 tests |
| 3 | `Caveats::new` renders for CFD too | `a_cfd_replay_gets_no_caveats_at_all` |
| 4 | `context_for`'s NotFutures early-return dropped | **survived at first** — see below |
| 5 | multiplier looked up on the instrument string, not the root | `context_for_builds_a_context_for_a_real_contract` |
| 6 | unknown contract treated as safe | `an_unlisted_contract_is_a_refusal_not_a_pass` |
| 7 | `is_problem` ignores `UnknownContract` | same |
| 8 | direction ignored (always Short) | 4 tests |
| 9 | arm-by boundary made exclusive | `the_arm_by_day_itself_is_still_armable` |

**Mutation 4 survived the first pass** and is the one worth recording. The
production entry point `context_for` had no test of its own — `Caveats::new`
caught the mutation downstream, so behaviour was safe, but only by a coincidence
of layering that no test asserted. A refactor trusting `context_for` alone would
have started printing a futures block on all 910 CFD fixtures. Fixed with
`context_for_refuses_a_cfd_instrument_at_the_entry_point`, plus a mirror
(`..._builds_a_context_for_a_real_contract`) so "always return None" cannot
satisfy it.

**Mutation 5 pins a real bug found by running the binary, not by a test.** The
first implementation looked the multiplier up on the plan's instrument string
(`GC 202612`); `instrument-lookup` has no contract-month dimension and keys the
row on the root (`GC`), so every probe answered CANNOT JUDGE. Unit tests passed
throughout — only the real binary showed it.

## Gate

- **2961 workspace tests pass** (2940 before), 0 failures.
- **910/910 golden fixtures pass, Net R +294.32** — unchanged, re-run after
  `cargo fmt` as well as before.
- `cargo clippy --workspace --all-targets` — no warning in any touched file
  (the remaining six are pre-existing, all in files this stage did not open).
- `cargo fmt --all --check` clean.
- Every rendered message checked through the **real binary** per
  `[[cargo_fmt_collapses_string_continuations]]` — all four close-out verdicts
  and both probe outcomes render with correct spacing.

## Prerequisite fixed on the way in

`oanda-client` was bumped to 0.3.0 in the shared sibling checkout, so all five
`^0.2.0` requirements stopped resolving and **nothing in this workspace would
build** — the primary checkout included. v0.3.0 is purely additive, so this was
a requirement bump with no code change (`81f48c7`).

## Status: COMPLETE
