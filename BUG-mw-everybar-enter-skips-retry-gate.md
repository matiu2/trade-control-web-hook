# Bug: an M/W (`EveryBar`) enter skips the retry gate, so nothing dedups its entries

**Severity:** Critical — live money. Three simultaneous positions on one setup,
3× the intended risk, with no gate anywhere refusing the duplicates.
**Component:** `core/src/dispatch/enter.rs` (the retry gate's entry condition)
and `cli/src/trade_patterns.rs` (the M/W enter builder).
**Found via:** EUR/GBP H1 M-top, TradeNation demo, 2026-08-20, plan
`m-eur-gbp-642f7851`.

**Status: FIXED** — `Intent::entry_dedup` (`core/src/intent/entry_dedup.rs`)
now states who owns entry dedup, instead of it being inferred from
`max_retries`. Regression tests: `mw_everybar_dedup_tests` in
`core/src/dispatch/enter.rs` (driven through the real `run_enter`) and
`mw_enter_hands_dedup_to_the_gate_with_a_one_placement_cap` in
`cli/src/trade_patterns.rs`.

## Summary

An M/W enter is `FireMode::EveryBar`: the engine fires it on **every** bar and
deliberately does not latch, because M/W recomputes its geometry from each new
shell and the resting entry order is meant to track it. The engine says so in
as many words (`engine/src/evaluate.rs`):

> A heartbeat (EveryBar) enter does not latch or finish the spine — the
> worker's run_enter owns the actual placement/dedup.

The delegation is correct. **The delegated owner was never invoked.**
`run_enter` entered the retry gate — the only thing that reconciles a fire
against this trade's prior attempts — on the condition
`max_retries != Static(0)`, and the M/W builder set exactly `Static(0)`:

```rust
// cli/src/trade_patterns.rs (before)
// Single-shot: a stop-out ends the setup. No re-entry, no preps.
intent.max_retries = trade_control_core::tunable::Tunable::Static(0);
```

So the enter fired every bar, skipped the gate, and placed an order each time.
Nothing else on the entry path asks "is one of these already live". Three bars
in a row placed, and all three filled.

The `trade-already-open` rejection at `core/src/retry_gate.rs` works perfectly
well — trade 145 proves it live. It was simply **never reached**.

## Broker evidence (TradeNation demo, 2026-08-20)

Plan `m-eur-gbp-642f7851`, EUR/GBP H1, M-top (short).

| bar | order | placed | outcome |
|---|---|---|---|
| 18:00 | `26936222` | 18:00 | filled |
| 19:00 | `26936430` | 19:00 | filled |
| 20:00 | `26936584` | 20:00 | filled |

Three orders, three fills, three positions held **simultaneously** on one
setup — 3× the intended risk on a plan whose author had written "single-shot".

It stopped at exactly three because of the **account-wide open-positions cap**
(`worker/src/secrets.rs`, default 3), enforced by both broker adapters
(`broker-tradenation/src/orders.rs`, `broker-oanda/src/oanda.rs`). That cap is
working as designed and is not the bug — it is a blunt account-wide backstop
that happened to be the only thing standing between this plan and unbounded
entries. Had the cap been higher, or had another instrument been holding a
position, the numbers would differ.

## Root cause: two correct decisions meeting in a gap

Neither half is wrong on its own. The bug lives in the space between them,
where **nobody owns entry dedup**.

**Decision 1 — the engine deliberately never latches.**
`engine/src/evaluate.rs`: the `AwaitEntry -> Done` transition requires
`FireMode::Once`, and M/W enters are `Trigger::MwEveryBar => FireMode::EveryBar`
(`tv-arm/src/trade_plan_build.rs`). Correct: an M/W resting order *should* be
re-priced each bar, so the engine must keep emitting fires. It explicitly
delegates placement dedup downstream.

**Decision 2 — the delegated owner is skipped.**
`core/src/dispatch/enter.rs` entered the gate only when
`max_retries != Static(0)`, and the M/W builder chose `Static(0)` to mean "a
stop-out ends the setup; never re-enter". Also correct, *as an answer to the
question its author was asking*.

**The defect is that `Static(0)` was overloaded.** It carried two meanings:

1. *"Do not re-enter after a stop-out"* — a **cap** on placements. The M/W
   author's intent, and true.
2. *"Skip the retry gate entirely"* — a statement about **dedup ownership**,
   which the gate-entry condition read it as.

For H&S the two coincide by accident: the enter is `FireMode::Once`, the engine
latches it, so it genuinely cannot fire twice and genuinely needs no gate. One
value answered both questions correctly, and nothing forced anyone to notice
they were different questions. M/W is the first pattern where they came apart,
and the moment they did, the answer to (2) was silently wrong.

## The fix

**Separate the two questions, and make the separation compile-time.**

A new field on the signed intent states the answer to question (2) outright:

```rust
// core/src/intent/entry_dedup.rs
pub enum EntryDedup {
    /// The engine's `FireMode::Once` latch guarantees a single fire —
    /// nothing to reconcile, skip the gate. (H&S single-shot.)
    EngineLatched,
    /// The enter can fire on many bars; the retry gate owns dedup.
    GateOwned,
}
```

The field is `Option<EntryDedup>`, and **`None` (absent on the wire) is not the
same as an explicit `EngineLatched`** — the distinction is load-bearing:

- **Present** ⇒ authoritative, obeyed as written in *both* directions. An
  explicit `EngineLatched` is never second-guessed from `max_retries`. This is
  what keeps the two questions genuinely separate at runtime; without it the
  answer would just be re-derived from the cap one layer down.
- **Absent** ⇒ a pre-field intent, healed at read time
  (`Intent::effective_entry_dedup`) by re-deriving the rule that *was* the
  authority when it was signed: single-shot stays engine-latched, **multi-shot
  becomes gate-owned**.

That healing is not optional. Without it, every plan already armed — and all
4041 enters in the fixture corpus, which are all `max_retries: 5` — would
deserialize to the bare default and **silently lose its gate on deploy**: the
exact bug, re-introduced for existing trades. Healing only ever *adds* the gate,
never removes it. It is deliberately not a serde default, so the stored body
stays a faithful record of what was signed (same posture as
`HeldTradeRecord::effective_holders`, v120).

- **`core/src/dispatch/enter.rs`** — the gate-entry condition now reads
  `verified.intent.effective_entry_dedup().needs_retry_gate()`. It no longer
  looks at the cap at all.
- **`cli/src/trade_patterns.rs`** — the M/W builder sets **both** halves
  explicitly: `entry_dedup: GateOwned` (reconcile every bar) and
  `max_retries: Static(1)` (one placement — a stop-out is still terminal).
  `build_enter_alert` emits `GateOwned` iff `max_retries > 0`, and leaves the
  field **absent** for single-shot so an H&S enter's wire form stays
  byte-identical.
- Because `Intent` is constructed by struct literal in ~20 places, adding the
  field is a **compile error** at every one until its author answers the
  question. A future pattern author cannot recreate the gap by picking a cap.

The field is skip-serialized when absent, so every pre-existing intent keeps its
exact wire bytes and (via the healing above) its historical behaviour. Signing
needs no change — `core/src/sig.rs` line-scans with an exclusion allowlist, so a
new field is signed and tamper-proof automatically.

### A second bug found while fixing this: cancel-then-reject at the cap

Enabling the gate for M/W exposed a **pre-existing latent rail violation** in
the shared gate, which the fix also had to close.

The gate's `AttemptState::Pending` arm **cancels** the prior resting order (on
the understanding the caller re-places it immediately), `break`s out of the
walk, and falls through to `attempts.len() >= max_retries`. The cancelled row
is still in `attempts` — nothing marked or removed it — so at `max_retries: 1`
the gate would **cancel the resting order and then reject the replacement at
the cap**: an order destroyed with nothing placed.

That is precisely the rail `order_control::reprice` states as *"never cancel an
order you cannot re-place"* — the same failure that forfeited a live EUR/CAD
setup on 2026-08-07 (resting limit `2318` cancelled, fire then rejected). The
2026-08 fix moved the gate **last** so no *other* gate could reject after the
cancel; it did not close the case where **the gate's own cap** rejects after
its own cancel.

It was latent because strategy-v2 runs `max_retries: 5` with slack, and **no
test covered the combination** (every cap test drove the walk with collapsed
states, so the cancel and the 429 never met). Under M/W it is not an edge case —
it is *every re-price bar*.

Fix: `EntryAttempt::superseded`. An attempt the gate cancels unfilled to
re-place is marked, and the cap counts **entries into the market**, not rows.
The mark is persisted (`StateStore::set_entry_attempt_superseded`, a `jsonb_set`
read-modify-write like `set_entry_attempt_broker_trade_id`) so it still holds on
the next bar. **No SQL migration** — the row is one `jsonb` body and the field
is `#[serde(default)]`.

While there, `next_attempt_no` was changed from `attempts.len() + 1` to
`max(attempt_no) + 1`. `attempt_no` is **row identity** (half the unique index
`(account, trade_id, attempt_no)`), not a counter; deriving it from a count
collides with a live row — which was **already true** after the sweep
hard-deletes an expired attempt, and would have become more reachable once the
cap started filtering.

## Resulting behaviour

```
bar 18:00  place stop @ 0.85778
bar 19:00  cancel 26936222, place @ 0.85781   (re-priced, ONE resting order)
bar 19:41  FILLED (one position)
bar 20:00  rejected: trade-already-open
```

Invariant: **at most one live entry order and at most one open position per
plan at any time.** A stop-out remains terminal — the gate walks past the
collapsed attempt and the cap of 1 rejects.

## Replay parity

The offline replay drives the same `run_enter` (via `core::dispatch::action`)
and the same `retry_gate::evaluate`, so the fix flows through structurally.

Note the replay previously stopped at one entry **only because its fake broker
has its own open-positions cap** (`cli/src/bin/replay_candles/replay_broker.rs`)
— masking, not agreement. That is why replay never surfaced this bug, and it is
worth remembering that **a green replay is not evidence about entry dedup**.

## Fixture corpus: 6 of 2759 diverge, and the divergence is CORRECT

⚠️ **The corpus is NOT green on this branch, deliberately.** The six cells were
left un-blessed pending sign-off — see "Open decision" below.

Diverged: `nzd-jpy-h1-2026-08-05-strategy-v2-{news-off,news-on}-entry-{limit,market,stop}`
— all six variants of one setup. Every other strategy-v2 cell (1344 of them) is
unchanged, so this is not a blanket behaviour change; it bites only where the cap
was actually the binding constraint.

Cause, verified against the **unmodified baseline**: that fixture hits
`retry-cap (5)` **22 times**, and the gate log names four prior attempts (#1, #2,
#4, #6) as `CANCELLED WITHOUT EVER FILLING`. Those four cancels are not
anomalies — in strategy-v2 they are the *designed* mechanism. From
`TradeSpec::strategy_v2`'s own docs:

> Whichever of the two fires first wins: **the worker's retry gate cancels the
> other's resting order** (both enters share this `trade_id` + a non-zero
> `max_retries`)

So a two-enter strategy-v2 setup supersedes a resting order almost every bar,
and each supersede was burning a `max_retries` slot. The cap — meant to bound
*entries into the market* — was instead being consumed by orders that never
entered it, starving the setup long before 5 real entries. Same defect as the
M/W case; a cap of 5 merely hid it where a cap of 1 could not.

With the fix the plan reaches attempt #8 and takes 4 legs instead of 2
(−2.00R → +1.23R on the `news-off-entry-stop` cell), with the open-position
backstop correctly refusing every duplicate throughout — so the extra entries are
sequential re-entries, never simultaneous positions.

### Open decision (NOT taken here)

Re-blessing these six is a **live-money behaviour change to strategy-v2**, which
is wider than the M/W bug this branch set out to fix. It was left for the
operator rather than decided unilaterally:

- The new numbers look better on this setup, but six cells is not evidence that
  loosening an effective cap is better *in general* — it is one setup that
  happens to fire often.
- The corpus is a **regression** gate, not a performance one. "It made more
  money" is not by itself grounds to move a golden.
- The alternative — count supersedes against the cap only for `GateOwned`
  intents with a cap of 1 — would keep strategy-v2 byte-identical, but that is
  a magic special case of exactly the kind this branch is removing, and it
  leaves the cancel-then-reject rail hole open for strategy-v2.

If the new behaviour is accepted, re-bless with
`replay-candles --test-mode --fixture <name> --fixtures-dir replay-fixtures --rebless`
(six cells). **`--rebless` writes only `expected.json`**, which matters here:
all six carry a hand-written `meta.json` `message` ("a cup and handle long with
a wildly swinging htf…") that a full re-save would destroy.

## Tests, and why the first set was worthless

Written before the fix, driven through the **real caller** (`run_enter`), and
asserting on observable broker traffic (what was placed / cancelled).

Mutation-tested. The first version of the M/W tests **survived reverting the
fix** — every one of them carried `max_retries: 1`, which the *old* condition
also admits to the gate, so they never isolated the variable under test. They
were passing for the wrong reason.

`the_gate_is_entered_on_entry_dedup_alone_not_on_the_cap` is the discriminating
test that fixes this: two arms with **identical `max_retries`**, differing only
in `entry_dedup`, asserting that only the `GateOwned` arm reconciles the resting
order. It fails the moment the entry condition goes back to reading the cap.

| mutation | result |
|---|---|
| gate-entry condition back to `max_retries != Static(0)` | **killed** (1 test) |
| M/W builder back to `Static(0)`, no `entry_dedup` | **killed** (2 tests) |
| cap counts superseded rows again (`attempts.len()`) | **killed** (3 tests) |
| `next_attempt_no` from the filtered count | **killed** (6 tests) |
| superseded mark never persisted | **killed** (2 tests) |
| healing drops a legacy multi-shot intent's gate | **killed** (2 tests) |
| healing overrides an EXPLICIT `EngineLatched` from the cap | **killed** (2 tests) |

The last two are why the field is an `Option`. An earlier version healed on the
bare enum, which made the healed value derivable from `max_retries` in every
case — mutation 1 **survived** against that design, because the old and new
gate-entry conditions were then behaviourally identical. Splitting
absent-from-explicit is what makes the separation real at runtime rather than
only at the type level.
