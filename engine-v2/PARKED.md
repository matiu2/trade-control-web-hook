# engine-v2 — PARKED (2026-07-27)

Development is deliberately **paused**, not abandoned, at a clean stopping
point. The branch (`feat/engine-v2-slice1`) is green (tests + clippy + fmt)
and pushed. Nothing is broken.

## Why parked

engine-v2's founding motivation was the replay↔live divergence bug class that
v1's `Phase` state machine kept producing (the `reversal` veto written live
but never in replay, terminal-veto-kills-close-guard, multi-shot plans
archived after the first fire). Since v2 was scoped, that class was largely
eliminated **in v1 directly**: the 2026-07 parity audit's four divergences are
all fixed, the ReplayBroker rewrite made replay a stateful held-ledger that
faithfully mirrors live, and v96 removed the last confirmed divergence at the
root. Meanwhile the operator's active goal is the staging promotion clock
(bug-free week → freeze week → $1k real), and every v2 hour competes with
that — worse, while v2 is alive every strategy change must land in THREE
places instead of two.

The remaining v2 work (reversal-close, signal detector/golden gating,
multi-shot, M/W, QM, the spread systems, a replay harness, worker
integration) is the *larger* half, and nearly all of it is re-porting
behaviour v1 already has and has already debugged.

## What's built (all tested, ~3,850 lines)

- The fact-based foundation: `Facts` blackboard, typed `FactKind`/`LineName`,
  `World` (no mode flag), pure `Rule::tick -> Vec<Effect>`, `tick_once`.
- Rules: break-and-close, retest (slope-tolerance), layered-preps enter,
  invalidate (too-high/too-low), pause (news standoff), expiry.
- The async executor (`c3521a2`): `Execution<EntryBroker, EntryStore>::
  drive_bar` — the async layer ABOVE the effects; executes `PlaceOrder` via
  `late_entry::resolve` (missed vs place-late) with fire-once stamped into
  both Facts and the store.
- Entry resolution (`49df430`): `PlaceOrder` carries a fully-resolved
  trigger/SL/TP/risk via `core`'s `Resolved::from_intent` (shared with v1, so
  the geometry math cannot drift).

## Resume point

The **news-reversal-close slice (System 2)** — full design in
`SCOPING-engine-v2-news.md`. It adds `Effect::ClosePosition` as a new arm on
`Execution::drive_bar`'s effect-walk (the path is established); the real work
is the reversal detector (reuse `core/src/signals/`, NOT Pine). After that:
multi-shot, then the replay harness to judge v2 by profit.

Before resuming, re-check `git log --since` on the v1 files listed at the
bottom of `SCOPING-engine-v2-news.md` — v1 moves weekly and any design map
goes stale.

## Resume signal

If v1's bug rate **stays high after the staging promotion week** — i.e. the
`Phase` machine keeps generating new bug shapes — that's the signal v2 is
still needed. If staging goes clean and promotes, v2 may never be needed,
and that is a fine outcome.

## Salvage (usable in v1 today)

- `engine-v2/src/late_entry.rs::resolve` — the missed-vs-place-late parity
  check — was already flagged as the right shape for v1's **spread-hour
  restore** case (resting orders pulled before the spread hour and restored
  after: same question). See the `late_entry_resolve_reuse_for_spread_hour`
  memory.
