# SCOPING — is the entry-decision logic worth simplifying?

> **STATUS (2026-07-22): option (a) SHIPPED as v110.** `SignalCriteria` +
> `admits` folded the 6 selection predicates into one value the enter builds from
> its intent; `first_confirmed_signal_at` arity 8→4; the `NeedsConfirmation`
> reason is now truthful (step 3 done). Behaviour-identical (UK 100 replay still
> Net R +1.47). Option (b) (per-leg sub-FSM) intentionally **not** done — see §2.
> This doc is retained as the rationale record.


Question from the operator after v109 (the fourth "confirmed-first scan forgot a
filter" bug): *the logic feels too complicated because it juggles two entry
systems; would an FSM (or two) decomplicate it?* This doc answers **"is it worth
it?"** with evidence before proposing **"how."** TL;DR: yes, a *small* refactor is
worth it, but it targets a different thing than "two entry systems," and a
full/dual-FSM rewrite is **not** worth it.

## 1. What the bug history actually shows

Every commit that ever touched `core/src/signals/state_machine.rs` (the whole
history — 7 commits):

| commit | kind | what |
|---|---|---|
| e7474bb | feature | port the Pine detector to Rust (Stage E) |
| 992a859 | fix | confirm only at end of `confirm_bars` window (v2.6) |
| d944919 | feature | enter on the FIRST confirmed signal, not the latest latch |
| 46a47f1 | **fix** | scope first-confirmed to **direction + floor** (+ QM drops golden) |
| c1bfdaf | **fix** | multi-shot QM re-entry advances past the consumed signal (**after** watermark) |
| 2359cf2 | chore | relocate plan-eval types to core |
| efe6378 | **fix** | first-confirmed skips a **non-golden** signal when enter needs golden (v109) |

Three of the fixes (46a47f1, c1bfdaf, efe6378) are the **same bug shape**: the
"which confirmed signal do I take" scan (`first_confirmed_signal_at`) needed a
new *selection filter* and a caller forgot it. The filters accreted one per
incident:

```
want_dir      (46a47f1 — took the wrong direction's signal: DE30)
not_before    (46a47f1 — took an ancient warmup-era signal: bug ①)
after         (c1bfdaf — re-took the already-consumed signal: QM multi-shot)
want_golden   (efe6378 — took a non-golden signal the enter rejects: UK 100 v109)
```

`first_confirmed_signal_at` now takes **8 positional args**, of which **6 are
selection predicates**:

```rust
first_confirmed_signal_at(candles, as_of, cfg,
    want_dir, want_kind, want_golden,   // ← predicates
    not_before, after)                  // ← predicates
```

The bug class is not "phase transitions are wrong" and not "the two entry systems
are tangled." It is: **the selection predicate is spread across N positional
arguments the caller assembles by hand, so each new predicate is a new chance to
drop one at a call site.** A missed predicate compiles fine and silently picks the
wrong signal.

## 2. Is "two entry systems" actually the problem? (Mostly no)

The operator's intuition was that BCR-stop (`05-enter`) vs Quasimodo-limit
(`09-enter-qm`) juggling drives the complexity. The code says otherwise:

- The two legs are **already decomposed**: `evaluate_entry` just loops over the
  plan's enter rules and runs each independently through `evaluate_one_entry`
  (engine/src/evaluate.rs:693). Two rows in `plan.rules`, evaluated in isolation.
  That part is clean.
- Both legs map to the **same** `Trigger::PinePattern` (tv-arm
  trade_plan_build.rs:295) and the **same** `eval_pine_entry`. There is no
  per-system decision code to untangle — v109 fixed *both* legs with one change
  precisely because they share the path.
- The only real "which system" branching in prod is the retest-gate key
  (`is_multi_enter` → `requires_preps` vs plan-global `is_retest`), and it appears
  in exactly **two** spots (evaluate.rs:879 and :1809), which are already
  factored behind `is_multi_enter`.

So an FSM-per-entry-system would **duplicate** the (correct, small) phase
machinery while leaving the (buggy) selection scan untouched — or worse, cloned
into two places that can now drift. That is the opposite of the goal.

## 3. Where the complexity really lives (sizes, measured)

- `engine/src/evaluate.rs`: 7201 lines total, **~2442 prod** (rest tests).
- `core/src/signals/state_machine.rs`: 1120 lines, ~560 prod.
- The existing FSM (`Phase::{AwaitBreakAndClose, AwaitEntry, Done}` +
  always-armed controls/guards, evaluate.rs:238) is **small and not the source of
  bugs** — none of the 7 scan commits touched phase logic.
- The complexity that bites is *below* the phase layer: signal **selection**
  inside `AwaitEntry`. That's `first_confirmed_signal_at` + `eval_pine_entry` +
  `pine_entry_dispatchable` + the coarse `enter_preconditions_by_leg` label
  (which caused the v109 "requires confirmation" red herring — it prints a static
  per-leg reason without checking whether a confirmed signal exists).

## 4. Options

### (a) `SignalCriteria` struct — RECOMMENDED, small

Fold the 6 selection predicates into one value the enter builds **once** from its
own intent, and have the scan take exactly that:

```rust
struct SignalCriteria {
    dir: Direction,
    kind: Option<SignalKind>,
    require_golden: bool,
    require_confirmed: bool,       // absorbs eval_pine_entry's confirmed_first
    not_before: Option<DateTime<Utc>>,
    after: Option<DateTime<Utc>>,
}
impl SignalCriteria {
    /// The ONE place intent-gates map to selection filters.
    fn from_enter(rule: &ConditionRule, state: &PlanState, floor: Option<DateTime<Utc>>) -> Self;
    /// The ONE predicate the winner-slot claim uses.
    fn admits(&self, t: &Tracked, print_time: DateTime<Utc>) -> bool;
}
```

`first_confirmed_signal_at(candles, as_of, cfg, &crit)` — arity drops from 8 to 4;
the winner claim becomes `if freshly_valid && crit.admits(t, print_time)`. A future
predicate is added in **two** obvious places (`from_enter` + `admits`) instead of
hunted across call sites, and the type system makes "did I pass all the filters"
into "did I build the criteria from the rule" (one typed call).

- **Effort:** ~1 fix's worth. Core + engine + the replay annotation. No wire/serde
  change, behaviour-identical (it's a mechanical fold of existing predicates).
- **Prevents:** exactly the recurring bug class — a forgotten selection filter.
- **Risk:** low; fully covered by the existing scan tests + the v109 real-data
  test. The `admits` predicate is a pure function, trivially unit-tested in
  isolation (each filter true/false).
- **Bonus (optional, cheap):** make `enter_preconditions_by_leg` call the *real*
  scan (via the criteria) so the "not-taken" reason stops being a static label and
  says the true cause ("no confirmed golden signal yet"). Kills the v109 red
  herring.

### (b) Per-entry-leg sub-FSM — NOT worth it

Give each enter leg its own small FSM. Reality check from §2: the legs are already
independent rows evaluated in isolation, and both share one selection scan. This
duplicates the phase machinery (not buggy), risks drift between the two clones,
and does **not** touch the selection scan (the actual bug site). High effort,
addresses the misdiagnosed cause.

### (c) Leave as-is

v109 stands; the code is correct today. But the accretion pattern is now 3-for-3:
the *next* selection predicate someone adds will likely reintroduce the same class
of bug at a call site. The struct in (a) is cheap insurance against a demonstrated
recurring failure — the case for doing *something* is stronger than "leave it."

## 5. Recommendation

Do **(a)**, skip **(b)**. The win is real and small, and it targets the *measured*
bug cluster (selection-filter accretion), not the *assumed* one (two entry
systems). Sequence:

1. Introduce `SignalCriteria` + `admits`, port `first_confirmed_signal_at` and
   `latched_signal_at`/`eval_pine_entry` onto it (behaviour-identical; all
   existing tests green).
2. Add per-filter unit tests for `admits` (each predicate independently).
3. (Optional) route `enter_preconditions_by_leg` through the criteria so the
   replay's "not-taken" reason is truthful.

Each step is independently shippable and green before the next — no big-bang
rewrite, and the shared engine path keeps replay == live throughout.

## 6. Open questions for the operator

- Is the truthful-reason bonus (step 3) wanted now, or defer? (It's cosmetic but
  it's the thing that made v109 confusing to read.)
- Any appetite to also collapse `want_kind` usage? (It's currently always `None`
  in prod — a latent filter with no live caller. Could drop it, or keep for the
  Pine parity it mirrors.)
