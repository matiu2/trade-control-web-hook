# BUG (NOT A BUG): M/W arm-time spread floor refuses where the fire-time gate would widen

**Status:** RESOLVED — working as designed. No code change. Recorded 2026-09-10
so the question is not re-opened a third time.

**Verdict:** The asymmetry is **deliberate**. M/W must NOT widen its stop-loss.

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
That re-check is meaningful only when TP is independent of SL. For M/W the R
re-check would be vacuous — R stays 1.0 no matter how far the stop moves,
because the TP moves with it.

**So for M/W there is nothing to salvage.** If the pattern's own stop is inside
10× the spread, the pattern is untradeable at that spread. Refusing to arm is the
correct outcome, and refusing at arm time (rather than signing a plan that can
only ever be rejected at fire time) is the useful place to do it.

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
  later to the **fire-time** path only. M/W was deliberately excluded, for the
  geometry reason above.

## If this is re-opened again

The question to answer first is: **does this pattern's TP depend on its SL?**
If yes (M/W), a widen is not a safety improvement — it is a different trade, and
the correct response to a too-tight stop is to refuse. If no (H&S), widen and
re-check R.
