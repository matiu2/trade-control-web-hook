# TODO — Stage 7: `UnitsBelowMinimum` parks instead of retrying forever

Stage 7 of the IBKR futures integration (plan:
`~/.home-claude/plans/magical-enchanting-bird.md`). The plan sequences this
**before** Stage 6b, because futures make a pre-existing bug acute: one contract
is 100% granularity, so a sub-minimum size stops being a rare edge.

Stages 2–5 are on `feat/contract-calendar` (`9d43289`, `f9034ef`, `1516bb1`,
`0ae38c0`), worktree `../trade-control-contract-calendar`.

## The bug

`EntryError::UnitsBelowMinimum` is raised by **both** existing brokers —
`broker-oanda/src/oanda.rs:243` (`if units == 0`) and
`broker-tradenation-adapter/src/lib.rs:802`. Neither is special-cased in
`core/src/dispatch/enter.rs`, so it falls through the generic `Err(err)` arm to
`ActionResult::Failed`, which `seen_decision` maps to `SeenDecision::Skip`.

Skip is deliberate and correct for `EntryTooCloseToMarket`: the seen-id must not
be poisoned so the next bar can retry. But `UnitsBelowMinimum` is a
**deterministic function of (equity, stop distance, multiplier)** — none of which
change within a bar. So the identical computation re-runs on every fire, fails
identically, and produces no operator signal. It never terminates and never
escalates.

## Design decision — the promote gate must become reason-aware

The plan says "add `StoredReason::BelowMinSize` and park instead of plain-fail".
Correct, but parking alone is **not sufficient**, and this is the part the plan
could not have known without reading the promote path:

`promote_due_orders` (`core/src/order_control/tick.rs:145-157`) gates **every**
parked order on one question — `clears_min_r`, computed from `sl_target` against
the current spread. A size-parked order has perfectly healthy geometry: its
`tp_distance / desired` clears `min_r` fine. So `clears_min_r` is `true` on the
very next tick and it promotes straight back into `UnitsBelowMinimum`.

And the order-control loop runs at the **frequent upkeep cadence**
(`worker/src/scheduler.rs:326-352`), deliberately faster than a bar. So a naive
park would convert a once-per-bar retry into a once-per-few-seconds retry —
**strictly worse than the bug being fixed**, while looking like a fix.

The `Broker` trait cannot answer "would this size now?" without placing: it
exposes no equity (sizing is private inside each broker's `place_entry`, by
design — `broker-oanda/src/risk.rs` is `mod`, not `pub mod`). So there is no
honest re-check to run every tick.

⇒ **`StoredVerdict` is decided per-reason.** `BelowMinR` keeps the spread
re-check unchanged. `BelowMinSize` promotes at most once per **new signal bar**
(`shell_time` advanced), which:

- restores the once-per-bar cadence the plan intends,
- keeps `drop_at` expiry, so it can't retry forever either,
- leaves an operator-visible parked row instead of silence,
- and re-drives through the full entry path, so equity/stop changes that make it
  placeable are picked up naturally.

## Tasks

- [x] `StoredReason::BelowMinSize` + `as_str` arm + serde round-trip test.
- [x] `stored_verdict` takes the reason into account (not a bare `clears_min_r`).
- [x] `park_stored_entry` grows a `reason` parameter (currently hardcodes
      `BelowMinR`).
- [x] The `UnitsBelowMinimum` arm in `run_enter` parks rather than plain-fails.
- [x] `promote_due_orders` passes the bar identity through.
- [x] README + CHANGELOG.

## Tests (the correctness anchors)

- [x] `UnitsBelowMinimum` parks; the outcome string says so.
- [x] A size-parked order does **NOT** promote on a same-bar tick — the
      regression that makes this a fix rather than an amplifier.
- [x] It **does** promote on the next signal bar.
- [x] It still drops at `drop_at` (can't retry forever).
- [x] `BelowMinR` behaviour is byte-identical — the spread re-check is untouched.
- [x] `EntryTooCloseToMarket` still plain-fails (Skip preserved).
- [x] Round-trip: `below-min-size` serde ↔ `as_str`.

## Mutation verification

Per `verify_new_analysis_code_by_mutation` — green tests prove nothing:

- [x] Make `BelowMinSize` use the `clears_min_r` gate ⇒ the same-bar test must go red.
- [x] Drop the `drop_at` check for the new reason ⇒ the expiry test must go red.
- [x] Park on `EntryTooCloseToMarket` too ⇒ the too-close test must go red.
- [x] `as_str` returns `below-min-r` for both ⇒ the round-trip must go red.

## Gate

`cargo test`, `cargo clippy`, `cargo fmt` before commit.

## What actually landed vs. the plan

The plan's shape (`StoredReason::BelowMinSize` + park instead of plain-fail) was
right, but **parking alone would have made the bug worse**, and that only shows
up when you read the promote path — see the design section above. The extra work
the plan didn't scope:

- `StoredVerdict` is decided **per-reason** (`StoredReason::rechecked_per_bar`).
- `StoredOrder.bar_seconds` — a size park needs a bar clock to tell "a new bar"
  from "the same bar, 5 seconds later". `skip_serializing_if`-elided, read
  fail-closed when absent (legacy bodies, the webhook path).
- `stored_verdict` takes a named `StoredCheck` rather than a second bare `bool`
  beside `clears_min_r` — two adjacent booleans transpose silently.
- `park_stored_entry` takes a `reason` (it hardcoded `BelowMinR`).

## Verified

- 2902 workspace tests pass (2889 before), 0 failures.
- `cargo clippy --workspace --all-targets` — no warnings in any touched file
  (remaining set is the same pre-existing one as Stages 4/5).
- `cargo fmt --all --check` clean.
- **8 mutations applied, 8 killed** — including a raw-instant bar comparison in
  place of bucketing, which reads as correct and is caught by 3 tests.
- The three operator-facing strings were **rendered by an actual binary**, not
  just asserted: `cargo fmt` collapses `\` continuations into literal spaces when
  the line fits, and tests never catch it.

## Replay parity

Unaffected, and checked rather than assumed: `ReplayBroker::place_entry` raises
only `RiskCapExceeded` / `OpenPositionsCapExceeded`, never `UnitsBelowMinimum`
(it reports `size: None` by design). No fixture can shift. The sizing divergence
itself is Stage 8's accepted, documented gap.

## Status: COMPLETE

Next is Stage 6 (6a `broker-ibkr` + `ibkr-client` promotion, 6b integral sizing
— which is what finally *consumes* the contract multiplier — and 6c
`BrokerHandle::Ibkr` + the ~16 cron match sites). Stage 6 is where the plan's
real risk sits, and it is mostly **external**: market-data entitlements, IBC
reliability, and whether the order path works at all against the paper Gateway.
