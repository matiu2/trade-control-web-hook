# BUG (NOT A BUG): M/W arm-time spread floor refuses where the fire-time gate would widen

**Status:** RESOLVED — working as designed. No code change. Recorded 2026-09-10
so the question is not re-opened a third time. **Explanation corrected
2026-09-16** (the verdict stands; two claims about the fire-time path were
wrong — see "The fire-time path" below).

**Verdict:** M/W must NOT widen its stop-loss, and does not: an M/W whose stop is
inside 10× the spread is refused, at arm time by an explicit check and at fire
time by the `min_r` floor. Operator's framing, 2026-09-16: *"If the stop needs to
be widened, then the trade should be cancelled for M and W."* That is what
happens.

---

## The observation that starts the investigation

Arming an M/W plan can fail outright with:

```
M/W stop-loss is too close to the spread: SL distance {d} < required {min}
(10× the {spread} spread). Tighten the spread (arm in a better session) or
widen the pattern.
```

`cli/src/trade_patterns.rs` (in `build_mw_pattern`). It fires at **arm time**,
before the plan is signed, so the failure is terminal — there is no plan to retry.

This looks like a defect next to the **fire-time** gate, which for the same floor
tries to *salvage* rather than reject (`core/src/dispatch/enter.rs`,
`widen_sl_to_spread_floor`):

> SALVAGE-BY-WIDENING: rather than reject a too-tight stop outright, try widening
> the SL to `SL_WIDEN_SPREAD_MULTIPLE`× the spread and re-check the trade still
> clears its R-floor.

Two enforcement points, same constant, different decisions. The arm-time comment
even claims "Same constant + decision as the worker" — the constant matches, the
decision does not.

## Why the asymmetry is correct: widening an M/W destroys the pattern

**An M/W trade is exactly 1R by construction.** `core::intent::mw_static_prices`
derives the take-profit as a reflection of the stop distance through the entry:

```rust
// W (long)
let entry = neckline + half_spread + half_pip;
let sl    = peak + half_spread - (spread + one_pip);
let tp    = entry + (entry - sl);          // <-- TP is defined BY the SL distance

// M (short)  — mirror image
let tp    = entry - (sl - entry);
```

The stop is anchored to the pattern's **peak** (`first_point`) and the target is
`entry ± (entry − sl)`. So R is not a tunable of an M/W setup — it is pinned at
1.0 by the geometry, and the SL is simultaneously the *minimum* and *maximum*
stop the pattern admits.

Widening the SL therefore does **not** produce the same trade with a safer stop.
It moves the TP by the same distance (TP is a function of the SL), which:

- pushes the target past the level the double-top/bottom actually projects, and
- means the thing being traded is no longer an M/W pattern.

The fire-time widen is safe for H&S precisely because H&S has an **independent**
fixed TP: widening the stop lowers R, `widen_sl_to_spread_floor` re-checks it
against `min_r`, and rejects if the widened stop can no longer hold the trade.
That re-check is meaningful only when TP is independent of SL.

**So for M/W there is nothing to salvage.** If the pattern's own stop is inside
10× the spread, the pattern is untradeable at that spread. Refusing to arm is the
correct outcome, and refusing at arm time (rather than signing a plan that can
only ever be rejected at fire time) is the useful place to do it.

## The fire-time path: M/W is NOT branch-excluded — it is rejected by the R-floor

⚠️ **Corrected 2026-09-16.** An earlier revision of this doc claimed (a) that M/W
was "deliberately excluded" from the fire-time widen and (b) that the R re-check
would be "vacuous — R stays 1.0 no matter how far the stop moves, because the TP
moves with it". **Both were wrong**, and the second inverts the actual mechanism.
The verdict — an M/W whose stop is inside 10× spread must not trade — is
unchanged and is what the code does. Only the explanation of *how* was wrong.

What actually happens at fire time:

- **There is no M/W branch at the widen.** `run_enter` resolves M/W through
  `Resolved::from_mw_intent` (`core/src/dispatch/enter.rs`, the `mw_effective`
  match), then falls through to the *same* `mut resolved` the widen mutates.
  Nothing near `widen_sl_to_spread_floor` tests for M/W.
- **TP does NOT move with the stop.** `mw_static_prices`
  (`core/src/intent/mw_resolution.rs`) computes `tp = entry ± (entry − sl)`
  **once**, at resolution. The widen then assigns `resolved.stop_loss` alone and
  leaves `resolved.take_profit` exactly where resolution put it.
- **So R genuinely falls below 1.0**, and `MIN_R_FLOOR` is exactly `1.0`. A
  widened M/W is therefore rejected `sl-widen-below-min-r` — it never reaches the
  broker.

The R re-check is not vacuous for M/W; it is the *whole mechanism*. An M/W is 1R
by construction, so **any** widen at all puts it under the floor. That is why the
outcome is right: the trade is refused, exactly as the operator intends.

### Why this is structurally safe, not a coincidence

The refusal depends on two things, and both are enforced rather than conventional:

1. **An M/W is exactly 1R.** `tp = entry ± (entry − sl)` — see above.
2. **`min_r` can never be set below `1.0`.** `Resolved` rejects an override under
   the floor outright (`ResolveError::MinRBelowFloor`, `resolution.rs`), so no
   signed intent can carry one.

Together: a widened M/W has R < 1.0 ≤ `min_r`, always. There is no configuration
in which the widen succeeds and places a broken-geometry M/W.

**If you are tempted to "fix" this**, don't — but do understand what you would be
changing. Adding an M/W branch that skips the widen is a *no-op on outcomes* (the
trade is refused either way) that trades a clear rejection reason for a different
one. Recomputing TP alongside the widened stop would be an actual behaviour change
and the wrong one: it would restore R to 1.0 and place a trade whose stop is no
longer at the pattern's peak — the exact thing this document argues against.

The one real cost of the current shape is **diagnostic**: the operator-facing
rejection reads `sl-widen-below-min-r`, which describes the arithmetic rather than
the cause ("this M/W's stop is inside 10× the spread, so the pattern is
untradeable right now"). If anything here is ever improved, make it that message.

## Corollary: H&S is not affected

H&S has no build-time SL at all — it anchors to the fire-time signal extreme, so
it never reaches this check and relies on the worker gate alone (where widening
*is* correct). This is stated in the originating commit `5ea86181`
("feat(entry): hard SL≥10×spread floor at fire time + M/W build time"), which
added the arm-time check for M/W only and deliberately did not give it a widen.

## The one thing that IS worth improving (cosmetic, not filed as a bug)

The arm-time comment's claim of "Same constant + decision as the worker" is
misleading — it invites exactly the reading that opened this investigation. The
decisions differ, correctly. If that comment is ever touched, it should say the
constant is shared and the decision is deliberately *not*, with the reason above.

Also note the arm-time spread is a **single instantaneous quote**
(`tv_arm::spread::read_spread_pips`), where the fire-time gate means over
`DEFAULT_SPREAD_WINDOW` (5) bars. `core/src/intent/sl_spread_floor.rs` documents
why a windowed read is preferable — a lone spiky quote can blow the floor out.
That does not change the verdict above (an untradeable pattern is untradeable),
but it does mean a momentarily-wide quote can refuse a setup whose recent mean
spread is fine. Re-arming a few minutes later is the operator workaround.

## Prior art

- `5ea86181` (2026-06-18) — introduced the floor: fire-time for all entries,
  build-time for M/W only. The M/W arm-time check never had a widen.
- The widen (`widen_sl_to_spread_floor` / `SL_WIDEN_SPREAD_MULTIPLE`) was added
  later to the **fire-time** path, which M/W also runs through. It carries no M/W
  branch; M/W is stopped there by the `min_r` re-check instead. (An earlier
  revision of this line claimed M/W was "deliberately excluded" — it is not. See
  the corrected fire-time section above.)

## If this is re-opened again

The question to answer first is: **does this pattern's TP depend on its SL?**
If yes (M/W), a widen is not a safety improvement — it is a different trade, and
the correct response to a too-tight stop is to refuse. If no (H&S), widen and
re-check R.

Second question, if you are looking at the *fire-time* path: **do not assume
there is an M/W branch guarding the widen — there isn't, and there does not need
to be.** Verify against `enter.rs` and `mw_resolution.rs` before concluding
anything about what M/W does there. This document was wrong about exactly that
for three months, which is the usual fate of a claim about code that lives only
in prose. See the repo memory `bug_doc_headers_go_stale`.
