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

- [ ] A. Close-out guard reachable from the replay path.
- [ ] B. Report warning naming the sizing gap.
- [ ] C. `--probe-account <amount>` granularity probe.
- [ ] Record the decision beside `PARITY.md` / `REPLAY-PARITY-AUDIT.md`.
- [ ] README section.

## Tests (the correctness anchors)

- [ ] A replay of a futures plan past its close-out is flagged.
- [ ] The same plan before its close-out is not.
- [ ] A CFD replay is completely untouched by all three features (the
      scope proof — every existing fixture must be byte-identical).
- [ ] The warning appears only when the run actually placed entries.
- [ ] The probe counts a floor-to-0 fill at a small account and none at a
      large one, on the same legs.
- [ ] The probe refuses (not silently 0) when the multiplier is unknown.

## Mutation verification

Per `verify_new_analysis_code_by_mutation` — green tests prove nothing:

- [ ] Guard passes everything ⇒ the past-close-out test goes red.
- [ ] Warning printed unconditionally ⇒ the CFD scope test goes red.
- [ ] Probe rounds instead of flooring ⇒ the small-account test goes red.
- [ ] Probe defaults a missing multiplier to 1.0 ⇒ the refusal test goes red.

## Gate

`cargo test`, `cargo clippy --workspace --all-targets`, `cargo fmt --all` before
commit. Golden fixtures must be re-run with `--fixtures-dir` (see
`[[replay_fixtures_dir_default_and_untracked_cells]]`) and must NOT move.

## Status: IN PROGRESS
