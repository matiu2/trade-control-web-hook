# BUG — replay cannot see broker-side entry recovery (`#19-10`), so 1339 corpus cells score a path they never run

**Status:** OPEN, 2026-09-09. Split out of
`BUG-entry-recovery-asymmetric-and-replay-blind.md` (Bug 2 of 2) so it can be
worked independently. Bug 1 of that doc (the asymmetric `--entry-stop`
recovery default in `tv-arm`) is being fixed separately — **the two touch
different code and do not collide**: Bug 1 is `tv-arm/src/hs_resolve.rs`
(what gets baked onto a plan), this one is the replay broker + fixture
schema (what replay does with it).

**Severity:** MEDIUM-HIGH. Silent live↔replay divergence on the entry type the
strategy actually ships with. Nothing in the corpus can currently detect it —
which is why it survived.

**Operator's call (2026-09-09):** *"Any live vs replay divergence is also a
bug."*

---

## Symptom

The live worker can be handed a stop-entry rejection by the broker, recover by
re-placing the order as a **market** or **limit**, and fill at a price the
offline replay never models. Replay scores the original order — or scores
nothing at all — and the two disagree with no warning on either side.

**1339 of 2695 corpus cells carry a configured `recover_entry` that replay
physically cannot execute:**

```
$ # every plan.json in the corpus, by entry axis
entry-limit    888
entry-market   222
entry-stop     222
unsuffixed       7
               ----
              1339   all of them: {"action": "stop"}
```

Those plans say "if this entry is wrong-side, recover to a stop". Replay
never does. The number is not a measure of harm — see "Blast radius" — but it
is the measure of how much of the corpus is scoring a configuration whose
recovery leg is unreachable offline.

## Cause — two independent gaps, both must be closed

### Gap A — the replay broker never raises the error

`core/src/dispatch/enter.rs:917` catches `EntryError::EntryTooCloseToMarket`
(TradeNation `#19-10`) and routes it to `place_entry_too_close_fallback`
(`:1396`), which consults the pure `recover_entry::recover_entry_plan`
(`core/src/recover_entry.rs`) and re-places as Market / Limit / Stop, or skips.

`ReplayBroker::place_entry` (`cli/src/bin/replay_candles/replay_broker.rs:961`)
**never returns that variant**:

```
$ grep -rn "TooClose" cli/src/bin/replay_candles/ | wc -l
0
```

So the entire `#19-10` branch is dead offline. This is *not* because the replay
broker refuses to model rejections in general — it already models two, with a
comment explaining the standard for including one:

```rust
// Enforce the two account caps the real broker enforces AND the replay
// can faithfully reproduce offline — so a live reject-at-cap is not
// silently taken as a fill (bug ③).
    EntryError::RiskCapExceeded { .. }        // percent risk cap
    EntryError::OpenPositionsCapExceeded      // open-position count
```

`EntryTooCloseToMarket` meets that same standard: it is a pure comparison of
the trigger against the market price, and the replay broker already knows both
(`req.entry.reference_price()` and its own `get_quote`, which backs the
defaulted `Broker::get_current_price` the recovery path reads). Nothing about
this rejection needs live state. It was simply never added.

### Gap B — nothing in the fixture could record it if it did

`Leg` (`cli/src/bin/replay_candles/economics.rs:114`) is the only per-position
record in `expected.json`:

```rust
pub struct Leg {
    pub entry_time: DateTime<Utc>,
    pub entry_price: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub exit_time: Option<DateTime<Utc>>,
    pub exit_price: Option<f64>,
    pub exit_reason: ExitReason,
    pub r: f64,
}
```

No order type. No recovery marker.

```
$ grep -rl "recover" replay-fixtures/*/expected.json | wc -l
0
```

**This is why Gap A alone is not a fix.** Add the rejection without adding the
field and a recovered entry becomes indistinguishable from an ordinary one that
happened to fill at that price — the fixture goes green either way and the
corpus still cannot fail on this. Both halves are required for the bug to be
*pinnable*, not just fixed.

## A third thing to check while you are in here

`place_entry_too_close_fallback` (`core/src/dispatch/enter.rs:1409`) opens with:

```rust
let trigger_price = match &resolved.entry {
    ResolvedEntry::Stop { trigger_price } => *trigger_price,
    _ => return Err(EntryError::EntryTooCloseToMarket),   // terminal
};
```

Only a **Stop** entry can recover. But `RecoverEntryPlan::Stop` — the "a *limit*
was wrong-side, re-place as a stop through the level" arm — exists in
`core/src/recover_entry.rs`, is fully tested there, and is exactly what those
888 `entry-limit` plans configure. From this caller it is **unreachable**: a
limit entry returns terminal two lines earlier.

Confirm before acting on it. Either the arm has another caller, or the `_ =>`
should admit `ResolvedEntry::Limit { .. }` and pass its price as the trigger.
Note this is the mirror image of Bug 1 (the tv-arm default) and probably wants
resolving in the same spirit — the operator's standing position is that these
rules should be symmetrical — but it is a *third* defect, in a third file, and
should be judged on its own evidence rather than assumed.

**CONFIRMED 2026-09-09 — the arm is genuinely dead.** Checked as instructed
rather than assumed:

```
$ grep -rn --include='*.rs' 'recover_entry_plan' .   # excluding worktrees/target
core/src/dispatch/enter.rs:1425                       # ← the ONLY caller
core/src/recover_entry.rs                             # def + its own unit tests
```

`place_entry_too_close_fallback` is the sole caller (`enter.rs:918`), and its
Stop-only guard (`:1409`) returns terminal for any non-`Stop` entry. So no live
input can reach `RecoverEntryPlan::Stop` — even though its handling arm is
**fully written** at `enter.rs:1527` (logs, sizing comment, `EntryRequest`,
`place_entry`, error handling: ~45 lines). This is finished, tested-at-the-pure-
layer, unreachable code. `recover_entry.rs`'s own tests keep it green forever,
which is exactly why nobody noticed.

That makes it the same shape as the headline bug — a well-tested pure layer whose
real caller is untested — and it is worth fixing in the same change. Treat the
`_ =>` admitting `ResolvedEntry::Limit { trigger_price }` as the likely fix, but
mutation-test it at the **caller**, not in `recover_entry.rs`.

## Blast radius — what is and is not known

**Known:** the path is never exercised offline (0 occurrences, corpus-wide), and
nothing recorded could show it if it were.

**MEASURED 2026-09-09 — live has never hit `#19-10` even once.** The doc asked
for this before choosing scope; here it is.

| source | window | `entry-failed: too-close-to-market` | any `entry-failed` |
|---|---|---|---|
| staging `request_records` | 2026-07-06 → 09-08 (19,872 rows) | **0** | 3 |
| dev `request_records` | 2026-06-26 → 09-06 (210 rows) | **0** | 0 |
| both worker journals | 2026-09-01 → 09-09 | **0** | 0 |

Method note for whoever re-runs this: a naive `LIKE '%too-close%'` /
`grep too-close` reports **15 rows and 2 journal hits — all false**. The worker
records each request's *body* into the outcome of the next `recording:` row, and
the plan JSON inside contains the string. Anchor the query
(`outcome = 'entry-failed: too-close-to-market'`, or grep for the emitting
module) or you will measure your own echo.

The 3 real rejections were all `entry-failed: broker rejected the order` —
generic `OrderRejected`, EUR_USD on **oanda**, within 3½ minutes on 2026-09-07.
Not disguised too-closes: the TN adapter maps `U::EntryTooCloseToMarket` to its
own distinct variant (`broker-tradenation-adapter/src/lib.rs:803`), so a genuine
`#19-10` could not arrive wearing the generic label.

**Why zero, and why that does not retire the bug.** `#19-10` is a *TradeNation*
rejection, and `broker-oanda` never constructs `EntryTooCloseToMarket` at all —
it emits only `AccountFetch`, `EquityParse`, `OpenPositionsCapExceeded`,
`OrderRejected`, `RiskCapExceeded`, `UnitsBelowMinimum`. So the measured zero is
partly a statement about *which broker has been trading*, not about how safe the
path is. Per `[[trade_both_brokers_forex_and_indices]]` the plan from 2026-09-06
is to trade **both** brokers — so TN volume, and with it the first real `#19-10`,
is ahead of us rather than behind.

**Revised severity: MEDIUM-LOW *today*, restoring to MEDIUM-HIGH the moment TN
carries live stop-entries.** Nothing in the journal is retroactively unsound —
the "every affected trade's as-designed R is unsound" worry the original
paragraph raised is **not** realised, because there are no affected trades. This
is a latent divergence, not an active one. It should still be fixed (the
operator's standing rule is that any live↔replay divergence is a bug), but it is
a *before TN goes live* fix, not a *drop everything* one.


## Fix sketch (not built — shape only, verify before following)

1. **Raise the rejection in the replay broker.** In
   `ReplayBroker::place_entry`, after the two existing cap checks: for a
   `ResolvedEntry::Stop`, compare the trigger against the as-of bar's market
   price and return `Err(EntryError::EntryTooCloseToMarket)` when the trigger
   has been overtaken (long: market ≥ trigger; short: market ≤ trigger) —
   mirroring the correct-side test `recover_entry_plan` already uses. Follow
   the existing cap checks' conservatism: reject only where replay can *know*,
   never guess.
2. **Record it on the leg.** Add the placed order type and a recovered marker to
   `Leg`, `#[serde(default, skip_serializing_if = ...)]` so the 2695 existing
   fixtures still deserialize unchanged. The concrete levels are already
   captured at placement (`PlacedLevels`), so this is plumbing, not new
   derivation.
3. **Pin it with a fixture** that actually recovers, plus unit tests either
   side.

## Verification — this one cannot be waved through on green tests

The whole reason this bug exists is that the corpus is blind to it, so *the
corpus passing proves nothing about the fix*. Required:

- **Mutation-test the entry point, not the layer below.** `recover_entry.rs` is
  already well covered and its tests will stay green no matter what you do to
  the replay broker. Delete the new rejection arm, or flip its comparison, and
  confirm a test goes **red**. A survivor here means the real caller is untested
  — which is precisely today's state.
- **Assert the elision.** Confirm the new `Leg` fields are absent from a
  re-serialized pre-existing fixture, and that all 2695 still deserialize.
- **Do not blanket `--rebless`.** Re-blessing can silently retire coverage, and
  a fixture whose recorded numbers change under this fix is telling you
  something. Split the test rather than blessing the new number.
- Hand-written `message` fields in `expected.json` are destroyed by a naive
  rewrite — `scripts/restore-fixture-messages.py` exists for this.

## Repro

```sh
# 1. replay never raises the error
grep -rn "TooClose" cli/src/bin/replay_candles/          # → 0 hits

# 2. nothing records a recovery
grep -rl "recover" replay-fixtures/*/expected.json       # → 0 files

# 3. but 1339 cells configure one
python3 - <<'PY'
import glob
n = sum('recover_entry' in open(f).read() for f in glob.glob('replay-fixtures/*/plan.json'))
print(n, "of", len(glob.glob('replay-fixtures/*/plan.json')), "cells configure recover_entry")
PY

# 4. the live-side strings to grep for in the worker journal
#    "entry-failed: too-close-to-market"
#    "too-close fallback: re-placing as MARKET"   (also LIMIT / STOP)
#    "too-close fallback: not recovering"
```

## Map of the code

| what | where |
|---|---|
| pure recovery decision (well tested) | `core/src/recover_entry.rs` |
| the caller — catches `#19-10`, re-places | `core/src/dispatch/enter.rs:917`, `:1396` |
| Stop-only guard (the possible third defect) | `core/src/dispatch/enter.rs:1409` |
| replay broker — needs the rejection | `cli/src/bin/replay_candles/replay_broker.rs:961` |
| fixture leg — needs the field | `cli/src/bin/replay_candles/economics.rs:114` |
| TN error mapping | `broker-tradenation-adapter/src/lib.rs:803` |

## Context

Found 2026-09-09 while measuring the new `--entry-matrix` axis
(`entry-rule-corpus-comparison.md`). The measured result there — limit ≈ stop,
because limits convert to stops — is what led to inspecting recovery at all.
Sibling: `BUG-entry-recovery-asymmetric-and-replay-blind.md`,
`BUG-stop-entry-recover-defaults-to-skip.md`.
