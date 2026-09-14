# TODO — pre-fill SL-breach sweep: wick semantics on both sides

Branch: `fix/sl-breach-wick-extreme`
Worktree: `../trade-control-web-hook-sl-breach-wick` (sibling — path-dep rule)
Base: `main` @ `6fa5c6d0` (identical to `staging`)

## The rule

**Has price TRADED past the stop since placement.** Wick semantics, over the
whole life of the resting order — not "is spot past the stop right now".

Operator's rationale: if we wanted to enter there and we didn't, and price went
the *other* way through our invalidation level, the plan for price is already
falsified. Entering now means entering a trade whose stop price has already
proven it can reach.

## The divergence being closed

| | before | after |
|---|---|---|
| live (`sweep.rs::maybe_breach_cancel`) | instantaneous spot quote on the ~900s upkeep loop | running adverse extreme persisted on the `EntryAttempt` row |
| replay (`fill_sim.rs`) | bar mid **close** (`c.c`) | bar **adverse extreme** (low for Long, high for Short) |

Shared `core::sweep_gate::breach_detected` is **unchanged** — it was always
correct. The bug was entirely in what each side fed it.

## Work

- [x] `core::sweep_gate::update_adverse_extreme` — pure fold (Long→min, Short→max, `None` seeds)
- [x] `core::sweep_gate::bar_adverse_extreme` — bar-grain analogue (Long→low, Short→high)
- [x] `EntryAttempt::adverse_extreme: Option<f64>` (`#[serde(default)]`, no migration — one jsonb body)
- [x] `StateStore::set_entry_attempt_adverse_extreme` + Mem / Pg / 2 test-stub impls
- [x] live sweep folds THEN judges the extreme; fail-soft on a failed write
- [x] replay `truncate_at_pre_fill_sl_breach` + `sweep_reason` read the bar extreme
- [x] cross-backend conformance case (Mem == Pg on the new setter)
- [x] mutation-tested at the entry points (4 injected; one initially survived — see below)
- [x] corpus run (report only, NOT re-blessed)

## Corpus result — exactly the predicted movement, NOT re-blessed

`replay-candles --test-mode --fixtures-glob '*' --check --fixtures-dir <main>/replay-fixtures`

```
matched 2847   succeeded 2833   failed(check-mismatch) 14
net R   before +1053.1756
        after  +1071.5006     delta +18.3250    cells worse: 0
```

All 14 moved cells are grid variants (entry × news × sl-anchor) of ONE chart
setup, `eur-cad-h4-2026-07-23` (iH&S long). Bit-for-bit the same as the
`EXPERIMENT-…` doc's `wick` arm.

Mechanism verified per-cell (`strategy-v2-news-off-entry-stop`):

| | legs | tp | sl | net R |
|---|---|---|---|---|
| blessed | 2 | 1 | 1 | +3.6900 |
| now | 1 | 1 | 0 | +4.6900 |

The suppressed leg entered and stopped out **inside one bar**
(`2026-07-28T13:00 → 13:00`, −1.00R). That bar's wick had already traded through
the stop before the entry triggered, so the resting order is now cancelled and
the losing re-entry never happens. The winning leg is byte-identical.

**+18.33 R is ONE observation, not 14.** 0.88% of taken cells. Do not read it as
a corpus-wide edge — see the experiment doc's Finding 2.

## Mutations injected at the entry point

Live side — injected, then `cargo test -p trade-control-cron sweep`:

| mutation | initially | killed by |
|---|---|---|
| direction-swapped fold (Long→max, Short→min) | DIED (3) | both `a_*_excursion_over_several_ticks_*`, `the_extreme_is_persisted_and_never_retracts` |
| judge `current` (spot) not `extreme` | **SURVIVED** | rewritten `a_recovered_{spot,short}_still_cancels_when_the_stored_extreme_breached` |
| never persist the extreme (`if false &&`) | DIED (2) | `the_extreme_is_persisted_and_never_retracts`, `a_legacy_row_with_no_extreme_is_not_treated_as_breached` |
| missing extreme reads as breached (seed ±∞) | DIED (2) | `a_legacy_row_with_no_extreme_is_not_treated_as_breached`, `the_extreme_is_persisted_and_never_retracts` |

Replay side — injected, then `cargo test -p trade-control-cli --bin replay-candles`:

| mutation | initially | killed by |
|---|---|---|
| both call sites back to `c.c` (close-sampling) | DIED (2) | `a_bar_that_wicks_through_the_sl_and_closes_back_is_a_breach` + its short mirror |
| `bar_adverse_extreme` hardcoded to the low | DIED (2) | `a_short_bar_that_wicks_…`, `pre_fill_sl_breach_blocks_a_later_fill_for_a_short` |

**Why the live #2 survived first:** the original test scripted
`[above, through, recovered]` across three ticks. The tick that *sees* the
excursion breaches under BOTH readings (spot is past the stop right then) and
the sweep is terminal, so tick 3 never ran. Fixed by separating the tick that
OBSERVES from the tick that ACTS — the row arrives already carrying its
excursion (as after a restart, or from a peer process) and the only quote that
tick sees is a recovered one. The multi-tick pair was kept alongside, since it
is what exercises the FOLD rather than the read.

**Why #2 survived first:** the original test scripted `[above, through, recovered]`
across three ticks. The tick that *sees* the excursion breaches under BOTH
readings (spot is past the stop right then) and the sweep is terminal, so tick 3
never ran. Fixed by separating the tick that OBSERVES from the tick that ACTS —
the row arrives already carrying its excursion (as after a restart) and the only
quote that tick sees is a recovered one.

## Out of scope — flagged, not built

- **Cadence.** `upkeep_secs` (900s) is a SHARED knob driving five loops
  (breakeven, blackout watch, blackout apply, sweep, order control). Dropping it
  to 60s is a 15× on all five, not a sweep-only change — it needs its own
  `sweep_secs` field first. Left alone deliberately.
- **Candle-feed sourcing.** `breakeven_watch` already pulls candles; the extreme
  could be folded from those bars instead of a per-attempt spot call, which
  would give wick resolution AND cut broker calls. Not built.
- **No positive evidence for the rule.** The fixture that would justify it
  (breached intrabar, later returns to trigger, would have filled) does not exist
  in the corpus. See `EXPERIMENT-pre-fill-sl-breach-sweep.md`.
