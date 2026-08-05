# BUG: `horizontal_cross` never fires when price is ALREADY past the level at window start

**Status:** **FIXED** 2026-08-05 — origin now seeded from the **arming bar** (`arming_bar` /
`seed_plan_state` in `engine/src/evaluate.rs`). See "Actual root cause" below: the reported root
cause was in the **wrong crate**.
**Severity:** High — a `setup_invalidation` veto silently never fires, leaving dead setups armed and
enterable.
**Affects:** live engine **and** replay (both reproduce).
**Reported:** 2026-08-05
**Reproducer plan:** `hs-nzd-chf-99d6bd00` (live) / `hs-nzd-chf-efef7f08` (replay), NZD/CHF H1 short.

---

## Actual root cause (supersedes the "Root cause" section below)

The reported analysis quotes **`engine-v2/src/cross.rs`**. That crate is **parked and not wired into
anything live** — the worker (via `trade-control-cron`) and the offline replay both use **`engine/`**.
`engine/` already implements the report's recommended "Fix A" (origin-side priming, from the XAU_USD
fix in `BUG-break-close-origin-seeded-from-warmup-bar.md`), so that fix would have changed nothing.
The observations in the report are all correct; only the root cause is misattributed.

The real defect is an **off-by-one in which bar fixes the origin**, and it only bites when price
crosses a level *during the arming bar*:

| bar (BNE) | stamp | open | close | vs 0.47583 | |
|---|---|---|---|---|---|
| 08-04 20:00 | `10:00Z` | 0.47580 | 0.47623 | below → **above** | **arming bar** — armed `10:01:28Z`, 88s in |
| 08-04 21:00 | `11:00Z` | 0.47622 | 0.47597 | above → above | first bar the engine evaluates |

Two individually-correct rules conspire:

1. A fresh plan must not back-fire on history, so `seed_plan_state` watermarks the newest back-window
   bar's **open**-time — the arming bar, still mid-flight. Its close lands *at* the watermark, never
   past it, so it is never evaluated as a live bar.
2. `origin_open` was deliberately not seeded there, because anchoring to the last *warm-up* bar let a
   whip-saw open poison it (the XAU_USD off-by-one).

Together they skip the arming bar and take the origin from the **next** bar's open — 0.47622, above
the cap. A directional settled `OnClose` `Up` cross needs `origin < level`, so `too-high` was dead for
the life of the plan, and `reseat_origin_on_dip` (the "must dip below first" contract) correctly saw
no dip until 08-05 09:00 — long after `09-enter-qm` fired.

Note the plan's `cross_buffer_pct` and `cross_buffer_atr` are both **0.0**: there is no band here, so
the buffer plays no part in this failure.

## Fix

`arming_bar(plan, candles)` — the bar whose span contains `armed_at` — seeds `origin_open` in
`seed_plan_state`. That bar is neither the pre-cursor warm-up bar (so the XAU_USD whip-saw fix stands,
a *different* bar) nor the first post-watermark bar (which is what fails here). It is the bar the
operator actually saw. `armed_at` is baked on the plan, so replay == live for free.

**Fallback:** a plan with no `armed_at` (or no bar covering it) seeds no origin and keeps the exact
pre-2026-08 behaviour. This is why every existing unit test and the one corpus fixture without
`armed_at` (`gbpaud-expiry-2026-06-19`) are untouched.

**Not changed:** the "must dip below first" contract. A plan armed when price is *genuinely* already
past a level still records a far-side origin and correctly reports no break — guarded by
`arming_bar_origin_does_not_relax_the_must_dip_first_contract`.

## Verification

- Corpus: **exactly 8 of 87 cells** changed — all `hs-nzd-chf-99d6bd00`, one per grid variant. The
  other 79 (including trendline necklines) are bit-identical. Re-blessed.
- The `strategy-v2-qm-market` cells' old goldens contain `09-enter-qm` firing `2026-08-05T01:00:00Z` —
  the bad live entry. Post-fix the plan retires at `11:00Z` and it never happens.
- New engine tests: `too_high_veto_fires_when_armed_already_above_and_never_dips` (the real bars),
  `arming_bar_origin_does_not_relax_the_must_dip_first_contract`,
  `plan_without_armed_at_keeps_the_first_live_bar_origin`.
- `all_fixtures_match_expected` now **collects** divergences instead of panicking on the first, so an
  intended behaviour change reports its whole blast radius in one run (it reported 1 of 8 before).

## Still open (not addressed here)

- **Arm-time guard:** should `tv-arm` refuse to arm a plan whose price is already past its own
  `too-high`/`too-low`? The margin here was **0.3 pips** — the fix is correct but rides on a
  borderline open, so a cheap arm-time sanity check is still worth having.
- **State-vs-transition semantics:** a plan armed when price is *genuinely* already past a cap still
  has a dead veto by design. Making `setup_invalidation` state-based (fire on any settled close past
  the level, ignoring origin) would cover that, but it is a separate product decision — it would need
  to split the shared prep/veto code path, since the must-dip-first contract is right for a *prep*.
- **`entry_level_vetos`:** `09-enter-qm` carried `{"level": 0.47583, "name": "too-high"}` and did not
  block an entry placed 5.8 pips past it. Independent of this fix; not investigated.

---

## Summary

`01-veto-too-high` at **0.47583** did not fire despite **twelve consecutive hourly closes above it**,
reaching **+12.6 pips** past the level. The plan stayed in `AwaitBreakAndClose` for the whole run and
remained enterable — it went on to fire `09-enter-qm` live while price was **5.8 pips above the level
that was supposed to have retired the plan**.

**Root cause:** `level_crossed`'s `OnClose` arm detects an **edge** (`prev < level && close >= level`),
not a **state**. If price is already above the level when the rule starts tracking, no such edge ever
occurs and the veto is permanently dead for the life of the plan.

---

## Evidence

### The level and the price

Trigger as armed:

```json
{
  "rule_id": "01-veto-too-high",
  "kind": "setup_invalidation",
  "intent": { "level": "close-positions", "name": "too-high" },
  "trigger": { "type": "horizontal_cross", "level": 0.47583, "dir": "up", "bar": "on_close" }
}
```

`armed_at`: **2026-08-04T10:01:28Z** (= 08-04 20:01 Brisbane).

TradeNation NZD/CHF H1 closes (Brisbane), pulled via `tv-mcp ohlcv --symbol TRADENATION:NZDCHF`:

| Bar (BNE) | Open | High | Low | Close | vs 0.47583 |
|---|---|---|---|---|---|
| 08-04 19:00 | 0.47562 | 0.47588 | 0.47540 | 0.47578 | below |
| **08-04 20:00** | **0.47580** | 0.47629 | 0.47570 | **0.47623** | **← the ONLY below→above edge** |
| 08-04 21:00 | 0.47622 | 0.47625 | 0.47585 | 0.47597 | above (+1.4p) |
| 08-04 22:00 | 0.47599 | 0.47634 | 0.47575 | 0.47608 | above (+2.5p) |
| 08-04 23:00 | 0.47607 | 0.47647 | 0.47598 | 0.47621 | above (+3.8p) |
| 08-05 00:00 | 0.47626 | 0.47643 | 0.47597 | 0.47604 | above (+2.1p) |
| 08-05 01:00 | 0.47605 | 0.47648 | 0.47596 | 0.47641 | above (+5.8p) |
| 08-05 02:00 | 0.47642 | 0.47706 | 0.47642 | 0.47695 | above (+11.2p) |
| 08-05 03:00 | 0.47694 | 0.47717 | 0.47688 | 0.47709 | above (**+12.6p**) |
| 08-05 04:00 | 0.47711 | 0.47715 | 0.47680 | 0.47687 | above (+10.4p) |
| 08-05 05:00 | 0.47684 | 0.47698 | 0.47676 | 0.47697 | above (+11.4p) |
| 08-05 06:00 | 0.47695 | 0.47702 | 0.47677 | 0.47687 | above (+10.4p) |
| 08-05 07:00 | 0.47642 | 0.47732 | 0.47622 | 0.47700 | above (+11.7p) |
| 08-05 08:00 | 0.47703 | 0.47706 | 0.47580 | 0.47643 | above (+6.0p) |
| 08-05 09:00 | 0.47644 | 0.47644 | 0.47520 | 0.47541 | back below |

**12 closes above. Veto fired 0 times.**

### Replay trace — the crossing bar is outside the window

```
Plan hs-nzd-chf-efef7f08 (NZD/CHF, H1) — 4 fire(s) over the window
  bar 2026-08-04 23:00:00 +10:00 phase=AwaitBreakAndClose     ← FIRST TRACED BAR
  ...
Done: false  |  final phase: AwaitBreakAndClose  |  fires: 4  |  Net R: +0.00
```

The replay's first bar is **08-04 23:00 BNE**. The crossing bar (**08-04 20:00**) is **three hours
earlier**, so by the time evaluation begins price is already above and there is no edge left to see.
Every subsequent bar is above→above.

### Live timeline — same outcome, different reason

```
2026-08-04 20:01 ⊙ register → ok
2026-08-05 00:45 • fired 01-pause-... (pause)
2026-08-05 01:00 • fired 09-enter-qm (enter)     ← price 0.47641 = 5.8p ABOVE the veto level
2026-08-05 08:45 • fired 02-resume-...
2026-08-05 08:45 • fired 01-news-start-...
2026-08-05 09:45 • fired 02-news-end-...
2026-08-05 19:00 • fired 07-close-on-sr-reversal (close)
```

Registered at 20:01 BNE — **one minute into the 20:00 crossing bar**. Nothing fired between
`register` and the 00:45 pause, so the crossing bar's close (21:00 BNE) was never evaluated with a
`prev_close` below the level. Same failure, arrived at from the other side of the boundary.

**Note `09-enter-qm` fired at 01:00 BNE anyway.** Its `entry_level_vetos` carry
`{"level": 0.47583, "name": "too-high", "past": "Above"}`, which should be an independent guard — it
checks the *derived entry level* (from `signal_low`), which evidently sat below 0.47583 even while
price traded above it. Worth confirming separately whether that guard is intended to catch this case;
if so, it is a second miss.

---

## Root cause

`engine-v2/src/cross.rs`, `level_crossed`, `BarEvent::OnClose` arm:

```rust
BarEvent::OnClose => {
    let Some(prev) = prev_close else {
        return false;                       // ← line 177: seed bar never fires
    };
    let upper = level + buffer;
    let lower = level - buffer;
    let up = prev < upper && candle.c >= upper;    // ← line 181: EDGE, not state
    let down = prev > lower && candle.c <= lower;
    match dir {
        CrossDir::Up => up,
        CrossDir::Down => down,
        CrossDir::Either => up || down,
    }
}
```

`prev` is sourced in `engine-v2/src/rules/invalidate.rs:112`:

```rust
let prev_close = w.facts.num_scratch::<LastClose>(&self.rule.id);
```

— a per-rule `LastClose` scratch that is **populated only by ticks the rule has actually processed**.
There is no seeding of the rule's initial above/below state at arm time.

Consequently, for `dir: Up`:

- **First evaluated bar:** `prev_close` is `None` → `return false` (documented behaviour: *"`None` on
  the seed bar, which never fires an `OnClose` cross"*).
- **Every bar after that, while price stays above:** `prev >= upper`, so `prev < upper` is false → no
  fire.

The veto can only ever fire if the engine happens to observe the exact below→above transition. Miss
that one bar and the rule is dead for the rest of the plan's life.

---

## Why this matters beyond one plan

A `setup_invalidation` with `level: "close-positions"` encodes **"this setup is invalid if price is up
here"** — a statement about *state*, not about a *transition*. A human reading this chart bins the
setup the moment price settles 10+ pips above the shoulder. The engine instead kept the plan alive and
enterable for twelve hours.

**Any plan armed while price already sits beyond one of its own veto levels has a permanently dead
veto.** That is not a rare configuration: it happens whenever price runs through the level in the gap
between the operator drawing the setup and the plan being armed — exactly the NZD/CHF case, where the
crossing bar closed **59 minutes after `armed_at`**.

**It also biases the existing fixture corpus.** Whether the bug is visible depends on where the replay
window starts relative to the crossing bar. Fixtures whose `--start` happens to precede the cross look
correct; fixtures starting after it silently under-report veto firings. Any historical analysis of
"how often did too-high fire" is therefore suspect until re-checked.

---

## Suggested fix

The semantics are a product decision — two candidates:

**A. Prime the cross state at arm time (recommended).** At plan arm / rule registration, evaluate
price against the level once and seed the `LastClose` scratch (or an explicit `side` fact) with the
resulting above/below state. Preserves edge semantics, makes live and replay agree, and fixes this
case with a minimal change. Requires a price sample at arm time — the arming candle's close is the
natural choice.

**B. Make `setup_invalidation` state-based.** Fire whenever a close is past the level, regardless of
the prior bar. Matches the "setup is invalid" reading directly, but changes behaviour for every
existing plan and would fire on the arming bar itself when price is already past — which may be
correct (arguably the plan should never have armed) but is a louder change.

**A is the smaller, safer change.** If B is preferred, it likely wants a guard so arming a plan whose
too-high is already violated fails loudly at arm time rather than instantly self-retiring.

Either way, a third question stands on its own: **should `tv-arm` refuse to arm a plan whose price is
already past its own too-high/too-low?** That would have caught this at the source.

---

## Secondary observation (separate issue, listed for context)

The replay window opened at **08-04 23:00 BNE** for a plan with `armed_at` **08-04 20:01 BNE** — a
three-hour gap that is itself what turned a catchable edge into an uncatchable one. Worth checking
whether the replay start should be derived from `armed_at` rather than wherever it currently comes
from; if it were, this specific instance would have fired correctly and the underlying bug would have
stayed hidden until a plan armed *after* a cross.

---

## Verification steps

1. Re-run the replay with `--start` at or before **2026-08-04 19:00 BNE** (one bar before the crossing
   bar). Expect `01-veto-too-high` to fire on the 08-04 20:00 bar's close. If it does, the edge logic
   is working and the defect is purely the missing initial-state seed.
2. Add a regression fixture: level `L`, first evaluated bar already closing above `L`, `dir: up`,
   `bar: on_close`. Assert the veto fires on the first bar.
3. Re-check the `09-enter-qm` `entry_level_vetos` path independently — it did not block an entry
   placed while price was 5.8 pips past the same level.
