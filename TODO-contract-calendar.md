# TODO — Stage 2: contract calendar + generator

Stage 2 of the IBKR futures integration (plan:
`~/.home-claude/plans/magical-enchanting-bird.md`). Produces the **close-out
deadline** table that Stage 3's arm-time guard reads.

Branch `feat/contract-calendar`, worktree
`../trade-control-contract-calendar` (sibling — `../` path-deps resolve
lexically, so it must not be nested).

## Why this exists

IBKR **force-liquidates** an expiring futures position without notice
(`SCOPING-ibkr-futures-broker.md` §3b-0). The deadline is **not** the contract's
expiry:

- **Long:** end of the 2nd business day before **First Notice Day**.
- **Short:** end of the 2nd business day before **last trade day**.

For COMEX gold FND is the last business day of the month *preceding* delivery,
so a **December** contract's long deadline is in **November** — ~a month before
the expiry date anyone would read off the chain. Verified live 2026-09-06: GCU6
(expiry 2026-09-28) was already past its long close-out deadline while still
listed as healthy front month.

## Design decisions

- **Pure arithmetic, no network.** Unlike `market-hours-gen` (which fetches
  candles), every rule here is a deterministic calendar computation. The
  generator is a renderer over a hand-maintained spec, so the whole thing is
  offline-testable.
- **Deadline is DIRECTION-DEPENDENT.** Long and short differ (FND vs last trade
  day), so the row carries both and the lookup takes a direction. Collapsing
  them to one number would silently over-permit longs on physical contracts.
- **Key on `(root, month)`, return `None` on ambiguity.** Do *not* repeat
  `core/src/spread_blackout/coverage.rs:87` / `core/src/intent/blackout/baked.rs:76`,
  which match on symbol alone with the first column bound to `_` — a colliding
  key silently takes the first row.
- **`None` is the fail-closed refusal signal**, never "no constraint".

## Tasks

- [x] Scaffold the crate as a workspace member.
  - [x] ⚠️ **Renamed to `contract-calendar-gen`.** The plan's suggested name
        `trade-calendar-maker` was **already taken by a real, unrelated
        project** at `/home/matiu/projects/trade-calendar-maker` — an
        economic-calendar / news-blackout tool that `cli` and `tv-arm` already
        depend on. `cargo add` refused with a lockfile collision. The dangling
        root symlink was just a broken convenience link to it, not a
        placeholder for this work. The new name also matches the sibling
        generators (`market-hours-gen`, `spread-baseline-gen`).
- [x] `holiday.rs` — US exchange holiday calendar + business-day arithmetic.
- [x] `rules.rs` — last-trade-day / FND / close-out derivation per contract root.
- [x] Deadlines are **direction-dependent** (long counts from FND, short from
      last trade day) — a single collapsed number would permit longs a month
      past their deadline.
- [x] `render.rs` — emit `core/src/contract_calendar_baked.rs`.
- [x] `bin/generate.rs` — CLI driver.
- [x] `core/src/contract_calendar.rs` — the `include!`-ing lookup module.
- [x] Wire into `core/src/lib.rs`.

## Tests (the correctness anchors)

- [x] **GC December ⇒ long deadline in November.** The single most important
      test in this stage — the month-early trap.
- [x] ES cash-settled ⇒ no FND; long and short deadlines coincide.
- [x] Unknown root ⇒ `None` (fail closed).
- [x] Business-day arithmetic across a US holiday (Thanksgiving, Good Friday).
- [x] Duplicate `(root, month)` key rejected at render time.
- [x] Ambiguous lookup returns `None` rather than the first match.
- [x] Malformed row (bad settlement / bad date / physical-without-FND) refuses
      rather than decoding to a defaulted entry.
- [x] Live fixtures from the paper Gateway 2026-09-06: GC 202609 last trade
      2026-09-28, MGC 202610 → 2026-10-28, ES/MES 202609 → 2026-09-18.

## Mutation verification

Per `verify_new_analysis_code_by_mutation` — green tests prove nothing. Break
the source, confirm red:

- [x] Flip FND from "last business day of PRIOR month" to "of the delivery
      month" ⇒ the GC-December test must fail.
- [x] Drop the 2-business-day close-out offset ⇒ deadline tests fail.
- [x] Remove a holiday from the table ⇒ business-day test fails.
- [x] Make ambiguous lookup return the first match ⇒ ambiguity test fails.
- [x] Collapse long/short into one deadline ⇒ 3 tests fail.
- [x] `is_past_close_out` answers "safe" for unknown contracts ⇒ test fails.
- [x] Unrecognised settlement defaults to cash ⇒ test fails.

⚠️ **Mutation 7 initially SURVIVED.** The generator makes duplicate keys
impossible, so no test could reach the lookup's ambiguity guard — it was
untested defence-in-depth, exactly the kind a later refactor deletes without a
red test. Fixed by splitting `lookup_in` to take an explicit table so the guard
runs against a synthetic ambiguous one.

## Open gaps (cannot be closed offline)

- **IBKR documents per-product close-out overrides** — *"certain contracts use
  a different time ahead of the Close-Out deadline as specified in the
  following table"*. That table is behind an anti-scraping block; unconfirmed
  whether GC/MGC/ES/MES override the standard 2 business days. An override
  would make these deadlines **too late**. Verify before real money; Stage 3's
  safety margin absorbs it meanwhile.
- **MGC settlement** — recorded `Physical` on the strength of CME's
  micro-metals literature (deliverable via an Accumulated Certificate of
  Exchange) and IBKR's own COMEX docs grouping MGC with GC. Several secondary
  sources say cash-settled; not followed. Error is asymmetric, so the
  conservative reading is also the better-sourced one.
- **Safety margin** (Stage 3's `deadline − margin`) still needs operator
  sign-off. Not this stage's code.

## Gate

`cargo test`, `cargo clippy`, `cargo fmt` before commit.

## Status: COMPLETE

All tasks and mutation checks done — see the mutation table at the bottom of
`contract-calendar-gen/README.md`. Stage 3 (arm-time refusal in
`build_trade_from_spec`) consumes `close_out_deadline()` from here.
