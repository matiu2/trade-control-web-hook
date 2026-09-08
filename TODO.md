# TODO — replay blind to broker entry recovery (`#19-10`)

Bug: `BUG-replay-blind-to-broker-entry-recovery.md`
Branch: `fix/replay-blind-to-entry-recovery`

Three independent defects, all in this change:

## 1. Gap A — replay broker never raises `EntryTooCloseToMarket`
- [x] `ReplayBroker::place_entry`: after the two existing cap checks, for a
      `ResolvedEntry::Stop` whose trigger has been overtaken by the as-of bar's
      market price, return `Err(EntryError::EntryTooCloseToMarket)`
- [x] Mirror `recover_entry_plan`'s correct-side test (long: mkt >= trigger,
      short: mkt <= trigger). Reject only where replay can KNOW.
- [x] Unit test at the REPLAY BROKER (6 tests; 4 mutations all killed) (the entry point), not recover_entry.rs

## 2. Gap B — nothing records a recovery on the leg
- [x] Add order-type + recovered marker to `Leg` (economics.rs)
- [x] `#[serde(default, skip_serializing_if=...)]` so 2695 fixtures still load
- [x] Assert the elision (mutations B1/B2 killed)
- [x] golden_eq comparator must SEE the fields (mutation B3 initially SURVIVED; test added)
- [x] Assert all 2695 still deserialize (corpus green)

## 3. Third defect — `RecoverEntryPlan::Stop` unreachable
- [x] `enter.rs:1409` `_ =>` must admit `ResolvedEntry::Limit { trigger_price }`
- [x] Mutation-test at the CALLER (2 mutations, each killed by a different test) (`place_entry_too_close_fallback`)

## Verification (bug doc is explicit: green corpus proves NOTHING here)
- [x] Mutation-test each new guard — 8 mutations, all killed (B3 initially SURVIVED → test added)
- [x] Did NOT blanket-rebless: diagnosed 1582 → comparator fix → 23 real ones
- [x] No meta.json rewritten, so no message restore needed
- [x] cargo clippy (0 new warnings; 16 pre-existing in engine/) + cargo fmt
- [x] Full workspace: 3023 passed, 0 failed
- [x] CORPUS mutation: deleting the rejection turns 23 cells RED (was: corpus blind)

## Corpus impact (MEASURED, not blessed blind)

First run after Gap A+B: **1582** cells diverged — all on the new `placed_as`
key alone, every economic number byte-identical. Fixed at the comparator
(expected-side-gated `placed_as`), NOT by re-blessing: down to **23**.

Those 23 are the real signal — each has **exactly one** `recovered_entry: true`,
i.e. a genuine `#19-10` recovery running offline for the first time. 3 setups ×
their axis variants + 1 spread-floor cell, overwhelmingly `entry-limit`:

| setup | expected R | with recovery | reading |
|---|---|---|---|
| cad-sgd (×8) | −1.66 / −1.00 | −1.00 | recovery AVOIDED 0.66R of loss |
| de30 (×6) | 2.83 / 1.53 / 2.41 | 2.12 / 1.20 / 1.76 | recovery COST ~0.7R of profit |
| nzd-jpy (×8) | −0.68 / −0.30 / −0.40 | unchanged | same net R, different entry leg |
| sgdjpy-spread-floor | −0.0806 | −0.0798 | marginal |

Verified de30 by hand: plan is `entry: limit` + `recover_entry: {action: stop}`
(one of the 888 cells the bug names). Old replay filled the limit @25732.5
(+2.83R); now the wrong-side limit is rejected and recovered as a STOP @25705.5
an hour later (+2.12R) — which is what the live worker would have done. The old
number was a fill live never got.

- [x] Diagnose all 1582 → comparator, not behaviour
- [x] Confirm the 23 are genuine recoveries (1 each), hand-verify de30
- [x] Re-bless ONLY those 23 (verified: exactly 23 dirs, only expected.json touched, sgdjpy `message` intact)
