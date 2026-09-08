# BUG — replay cannot see broker-side entry recovery (`#19-10`), so 1339 corpus cells score a path they never run

**Status:** **FIXED 2026-09-09** (branch `fix/replay-blind-to-entry-recovery`).
All three defects closed; see "Resolution" at the foot. Originally split out of
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

## Blast radius — what is and is not known

**Known:** the path is never exercised offline (0 occurrences, corpus-wide), and
nothing recorded could show it if it were.

**Not known:** how often live actually hits `#19-10`, and what it cost. That is
answerable and worth answering before choosing how far to take the fix — the
worker logs a distinct string for exactly this reason
(`recover_entry::outcome_for_entry_error` → `"entry-failed: too-close-to-market"`,
and on recovery `"too-close fallback: re-placing as MARKET|LIMIT|STOP"`). Grep
the staging worker journal and the `request_records` outcomes. If it fires
regularly, every affected trade's as-designed R in the journal is unsound and
that should be stated in the fix.

Do not skip this measurement on the grounds that the code fix is obvious. The
sibling bug's lesson was that an unmeasured "surely this matters" and an
unmeasured "surely this doesn't" are the same mistake.

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


---

## Resolution (2026-09-09)

All three defects fixed on `fix/replay-blind-to-entry-recovery`.

### Gap A — replay now raises `#19-10`

`ReplayBroker::place_entry` gained a third rejection alongside the two existing
account caps, with the same conservatism (reject only where replay can *know*):

```rust
if let Some(market) = self.market_price_as_of()
    && wrong_side_of_market(&req.entry, req.direction, market)
{ *self.recovering.borrow_mut() = true; return Err(EntryError::EntryTooCloseToMarket); }
```

`wrong_side_of_market` is a pure helper. **The two pending types invert** — a
stop rests above the market (long), a limit below — so they get separate arms and
separate tests; conflating them would reject every healthy limit in the corpus.
A `Market` entry has no resting level and is never wrong-side. No market price
(no bar at/before `as_of`) ⇒ **no rejection**, never a guess.

The rejection deliberately does **not** consume the armed placement: the
dispatcher's recovery re-place is the same intended entry and must land on the
same armed slot and order id.

### Gap B — the leg records it

`Leg` gained `placed_as: Option<PlacedOrderKind>` and `recovered_entry: bool`,
both elided when absent/false so the 2695 pre-existing goldens round-trip
unchanged. Plumbed `PlacedLevels → HeldOrder → HeldPosition → ClosedTrade →
RealizedOutcome → FireResult → Leg` (every hop compile-enforced).

⚠️ **The `golden_eq` comparator had to change too, and this is the subtle part.**
Adding the fields is useless if the tolerant comparator ignores them — a
recovered entry would still compare equal to an ordinary one at the same price.
But comparing them symmetrically failed **1582 cells whose every economic number
was byte-identical**, purely on the new key. Resolution: `placed_as` is compared
only when the **expected** side recorded one (`a.placed_as.is_none_or(..)`) —
absent means *not recorded*, which has nothing to disagree with, while a golden
that *does* record one is held to it exactly. `recovered_entry` is a plain bool
with a meaningful `false`, so it is always compared.

### Third defect — `RecoverEntryPlan::Stop` was unreachable

Confirmed dead, and worse than the doc supposed: `recover_entry.rs` has **no
tests for the `Stop` arm at all** (17 tests cover Skip/Limit/Market). So it was
unreachable *and* untested, while its ~45-line handling arm sat fully written.

`place_entry_too_close_fallback`'s guard now admits `ResolvedEntry::Limit`
alongside `Stop` (both carry a resting level; `Market` stays terminal), matching
what `EntrySpec::Limit::recover_entry`'s own docs already promised.

**Note the doc's geometry prose here is backwards** (`recover_entry.rs:150-152`,
carried into this bug report): a long limit goes wrong-side when the market falls
*through* it (`current <= trigger`), not when price runs up. The code was right;
only the comment misleads. Two tests were written against the wrong reading and
corrected — worth knowing before trusting that comment.

## Measured corpus impact

First full run: **1582** cells diverged — all on the new key alone. Fixed at the
comparator, **not** by re-blessing → **23**.

Those 23 each carry **exactly one** `recovered_entry: true` — a real `#19-10`
recovery running offline for the first time. 3 setups × axis variants + 1
spread-floor cell, overwhelmingly `entry-limit`:

| setup | was | with recovery | reading |
|---|---|---|---|
| cad-sgd (×8) | −1.66 / −1.00 | −1.00 | recovery **avoided** 0.66R of loss |
| de30 (×6) | 2.83 / 1.53 / 2.41 | 2.12 / 1.20 / 1.76 | recovery **cost** ~0.7R of profit |
| nzd-jpy (×8) | −0.68 / −0.30 / −0.40 | unchanged | same net R, different entry leg |
| sgdjpy-spread-floor | −0.0806 | −0.0798 | marginal |

Hand-verified de30: the plan is `entry: limit` + `recover_entry: {action: stop}`
— one of the 888 cells this bug names. Old replay filled the limit @25732.5
(+2.83R); it now rejects the wrong-side limit and recovers as a **stop** @25705.5
an hour later (+2.12R), which is what the live worker would have done. **The old
number was a fill live never got** — the divergence, made visible.

Re-blessed **only** those 23 (verified: exactly 23 dirs changed, only
`expected.json` touched, no `meta.json` rewritten, the hand-written
`sgdjpy-spread-floor-min-r-block` `message` intact).

⚠️ That cell's `message` ("all three entries blocked… nothing fills") was
**already stale before this change** — the committed golden had 2 legs. Left
alone: not this bug's drift, and rewriting it would destroy hand-written text.

## Verification — the corpus can now fail on this

The bug's own standard was that a green corpus proves nothing. So:

- **8 mutations, all killed.** Guard reverted / long-stop comparison flipped /
  limit given the stop comparison / no-price conservatism dropped / each
  `skip_serializing_if` dropped / comparator gate removed — each turns a test red.
- **One mutation initially SURVIVED** (`golden_eq` ignoring the new fields). That
  is the "a survivor means the real caller is untested" case
  (`[[mutation_test_the_entry_point_not_just_the_layer_below]]`) — a test was
  added and the mutation now dies.
- **The decisive one: delete the replay rejection and 23 corpus cells go RED.**
  Before this change the corpus was blind to the entire path; it now pins it.
- Corpus-wide, goldens recording a recovery went **0 → 23**.
- Full workspace: **3023 passed, 0 failed**. Clippy clean (16 pre-existing
  `engine/` warnings, unchanged). `cargo fmt` applied.

## Still true after the fix

The **live** measurement is unchanged: `#19-10` has never fired in production
(0 occurrences across 19,872 staging + 210 dev `request_records`, Jul 6 → Sep 8).
`broker-oanda` cannot even construct the variant. This fix closes a **latent**
divergence before TradeNation carries live stop/limit entries — which the
2026-09-06 both-brokers decision puts ahead of us, not behind.
