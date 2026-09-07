# TODO — Stage 3: arm-time close-out refusal

Stage 3 of the IBKR futures integration (plan:
`~/.home-claude/plans/magical-enchanting-bird.md`). **THE GATE** — nothing that
can place a futures order lands before it.

Stage 2 (the calendar + generator) is complete and pushed as `9d43289`; its
TODO is preserved in that commit.

Branch `feat/contract-calendar`, worktree `../trade-control-contract-calendar`.

## What this stage does

Refuse to arm an IBKR futures plan whose `trade_expiry` runs past the
contract's close-out deadline (minus a safety margin), and refuse outright when
the contract is unknown. IBKR force-liquidates without notice; an armed plan
that can still be entering positions inside the close-out window is the failure
mode.

## Ordering problem found on entry (resolved)

The plan says the guard applies "only when `broker == Ibkr`", but
`BrokerKind::Ibkr` does not exist until **Stage 4**. Rather than reorder the
stages (Stage 4 is a ~400-line enum sweep, and landing the gate after it would
violate "nothing order-placing lands before the guard"), the guard is written
**broker-agnostic and driven by the instrument**: it fires when the instrument
parses as a futures contract (`ROOT` + month code, e.g. `GCZ6`/`GC 202612`),
which no CFD/spot instrument does. Stage 4 then adds the `BrokerKind::Ibkr`
check as a *narrowing* condition, not a rewrite.

This is strictly safer than the plan's shape: today a futures-looking
instrument on any broker is refused, so the gate cannot be bypassed by an
un-migrated broker field.

## Design decisions

- **Reuse `core::intent::Direction`**, don't keep the parallel
  `contract_calendar::Direction`. Two identically-shaped enums for "which side
  of the market" in one crate is the drift hazard the four parallel broker
  enums already demonstrate. Stage 2 introduced the duplicate; collapse it now
  while it has exactly one consumer.
- **Direction comes from the pattern**, via the existing
  `TradePattern → Direction` mapping (H&S/M ⇒ Short, iH&S/W ⇒ Long). The
  deadline differs by ~a month on physical contracts, so this must not be
  guessed.
- **Unknown contract ⇒ refuse.** `lookup` returning `None` is a refusal signal,
  never "no constraint" — that is the whole fail-closed contract of Stage 2.
- **`Strict` only.** Offline `--plan-out` replay of a historical setup must
  still build, exactly as it does for an expired `trade_expiry`.

## Tasks

- [x] Collapse `contract_calendar::Direction` onto `core::intent::Direction`.
- [x] `cli/src/futures_symbol.rs` — parse an instrument into
      `(root, contract_month)`; `None` for anything that isn't futures.
- [x] Safety margin constant + business-day arithmetic reachable from `core`.
- [x] The guard itself in `build_trade_from_spec`, at the `trade_expiry` seam.
- [x] README section.

## Tests (the correctness anchors)

- [x] GC plan whose window runs into the close-out ⇒ operator rejection
      (an `Err`, not a panic).
- [x] The same plan 30 days earlier ⇒ builds.
- [x] Unknown futures contract ⇒ refuses.
- [x] A **long** and a **short** on the same physical contract get different
      verdicts in the month between the two deadlines. This is the
      month-early trap reaching the operator.
- [x] An OANDA/CFD plan with identical geometry is unaffected (proves the
      guard is scoped, per the plan's verification section).
- [x] `Lenient` (`--plan-out`) still builds.

## Mutation verification

Per `verify_new_analysis_code_by_mutation`:

- [x] Drop the safety margin ⇒ a boundary test must fail.
- [x] Treat unknown contract as safe ⇒ the unknown test must fail.
- [x] Ignore direction (always long / always short) ⇒ the pair test must fail.
- [x] Apply the guard in `Lenient` too ⇒ the plan-out test must fail.
- [x] Make the symbol parser accept a CFD name ⇒ the OANDA test must fail.

## Gate

`cargo test`, `cargo clippy`, `cargo fmt` before commit.

## Open gaps carried into Stage 4

- **The guard is instrument-keyed, not broker-keyed.** Stage 4 should add
  `broker == Ibkr` as a *narrowing* condition once `BrokerKind::Ibkr` exists —
  not replace the instrument check, which is what stops a futures contract
  slipping past on an un-migrated broker field.
- **The instrument spelling for futures is not yet fixed anywhere else.**
  `instrument-lookup` has no contract-month dimension (scoping §3b), so
  `futures_symbol` is currently the only reader of the contract month. Stage 5
  bakes the multiplier and will need the same key; keep the two agreeing.
- **The safety margin (10 business days) has not been signed off by the
  operator.** It is a one-constant change plus a regenerate if they want
  another number.

## Status: COMPLETE

Stage 3 is the gate; nothing order-placing may land before it. Next is Stage 4
(`BrokerKind::Ibkr` + the parallel-enum sweep), which is what makes
`trade-control-accounts`, `tv-arm-staging`, `trade-control-staging` and
`journal-staging` IBKR-aware.
