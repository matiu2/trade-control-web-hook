# PARKED: the pinbar right-hand pivot — measured and rejected 2026-09-16

**This branch is a completed experiment, not unfinished work.** It is kept
because the same idea will occur again, and the measurement is worth more than
the code. Nothing here was ever merged; `main` and `staging` are clean.

**Decision: pinbars keep the LEFT-side pivot test only** — `low < low[1]`
(long) / `high > high[1]` (short), `core/src/signals/detect.rs:220,226`, which
is long-standing pre-existing behaviour. No code change shipped.

## What was asked

> "with the pinbars, they should only be marked if they are a pivot point. For a
> long pinbar, the low must be lower than both the left and right bars. If it is
> the last bar, it's considered 'pending' still."

and later, framing the trade-off:

> "we're here to make money, and making money beats correctness"

with the constraint that the chart and the engine must not disagree.

## Why the rule splits in two, and why that matters

The right-side test **cannot** live in `detect_at`: when the pinbar closes, bar
N+1 does not exist yet. So it necessarily splits into two independent changes,
and they have completely different costs:

| half | where | what it affects | cost |
|---|---|---|---|
| **A** | `signals/state_machine.rs`, `update_tracked` at `bars_elapsed == 1` | marks the pinbar `Invalid` at N+1 — display / signal lifecycle | **0.00R** |
| **B** | `signals/print_gate.rs` + `engine/src/evaluate.rs` | defers a plain pine pinbar *enter* from the print bar to N+1 | **loses money** |

Half A is free because the engine **already** enforced ~97% of the rule by a
different route: the existing breach rule (`c.l < t.low` → `Invalid`) kills any
signal whose own extreme is taken out, and for a long pinbar "bar N+1 made a
lower low" *is* that breach. Measured over 86 distinct candle sets / 5141
pinbars: 1566 failed the right-hand pivot, 1521 were already dead on that very
bar, 24 died later in the window, and **all 21 survivors were exact ties**
(`c.l == t.low`, where `<` is false but `>` is also false). None sat on a trade.
So half A moves **0 of 2847 corpus cells** — the operator's rule was almost
entirely already in force.

Half B is the only part that changes trading, and it is the part that costs.

## The measurement that decided it

`skip-bcr` + `entry-stop` (the live entry technique), news-off, all three SL
anchors — 111 cells, 13 movers:

| SL anchor | setups | before | after | delta |
|---|---|---|---|---|
| default | 59 | +40.72 | +31.24 | **−9.47** |
| fib-top | 26 | +17.26 | +10.64 | **−6.62** |
| invalidation | 26 | +23.22 | +16.61 | **−6.62** |

**The decisive number is not the total — it is the asymmetry:**

- sum of all positive movers: **+1.64R**
- sum of all negative movers: **−24.35R**
- worst mover −6.11R, best mover **+1.00R**

The whole upside available to this rule, across the entire entry technique, is
+1.64R.

Win/loss split on the default anchor makes the mechanism unmistakable:

| | taken | wins | losses |
|---|---|---|---|
| before | 46 | 29 (+59.68) | 17 (−18.96) |
| after | 46 | 29 (+50.18) | 17 (−18.94) |

Same 46 trades, same 29 winners, same 17 losers, **losses identical to the
cent**. The entire cost came out of *winning* R. The rule avoided **zero**
losses — the premise ("filter fake pinbars, dodge their losses") is false here,
because the fakes were already being filtered.

## Why it is rejected on SHAPE, not on the point estimate

Be careful with the total: collapsed to 9 distinct setups the movers are 4
negative / 5 positive, median +0.03R, **sign test p = 0.75**. Two setups
(`aud-nzd-h1-2026-07-21` −13.14, `eur-cad-h4-2026-07-23` −9.19) carry 98% of the
loss. The *mean* is statistically indistinguishable from zero and this corpus
cannot pin it.

What IS structural is the payoff shape, and it follows from the mechanism:

- a **stop** entry needs price to trade back through the level;
- wait a bar and on the trades that ran hardest it never does;
- so the downside is *the whole runner*, while the upside is capped at part of a
  saved stop-out.

Bounded gain, unbounded tail, concentrated in the best trades. That holds
regardless of where the mean sits, which is why more data would not rescue it —
it would only sharpen a number that is already the wrong shape.

## A scoped variant was also tried, and also failed

`44ff3dc3` scopes the deferral to **re-entries only** (first entry still fires
on the print bar), on the theory that the gains came from suppressing bad
multi-shot re-entries while the losses came from delaying first entries.

Live config: +40.72 → **+32.24** (recovers 1.0R of 9.5R) **and turns a winner
into a loser** (29/17 → 28/18).

It fails because *the theory was wrong*. The delay damage was never
first-entry-specific: in the variant's delayed-fill bucket, cells with a
stop-out carry −38.03R of −42.60R. Deferring a re-entry suppresses the bad ones
(+36.05R) and degrades the good ones (−38.03R) by almost exactly as much.

## Analysis traps hit during this investigation — read before repeating it

1. **The first measurement read +0.00R and was nearly reported as "free".** Half
   A alone does not touch the entry path at all: a plain pine enter takes the
   **print-only** path (`engine/src/evaluate.rs`, `sig.signal_bar_time ==
   candle.time`) and gates only on `needs_golden` / `needs_confirmed` —
   **nothing reads `SigState`**. The pivot resolving at N+1 happens *after* the
   entry was already placed. An unreached rule and an inert rule look identical.
   **Always run a destructive sanity mutation first**: rejecting every plain
   pinbar enter moves −93.46R; rejecting every re-entry moves −222.24R. That is
   what proves the corpus reaches the code.

2. **`replay-candles --baseline`'s movement report is broken.** It reported "55
   moved (32 improved, 23 worse)" against a baseline blessed by the *same
   binary*, reproducibly, where a field-by-field JSON diff showed **zero**
   differences (exact f64 compare, `cli/src/bin/replay_candles/baseline.rs:420`,
   printed to 2dp so a phantom delta looks real). Bless two baselines and diff
   the JSON in Python instead.

3. **Cross-column corpus totals are not money.** Each setup expands to ~72 cells
   that are *mutually exclusive configurations of one trade* (4 entry rules × 3
   entry styles × 3 SL anchors × news on/off). `aud-nzd-h1-2026-07-21` reported
   as "+91.87 → +13.28 = −78.59R" when the live-config reality was
   "+5.08 → +2.11". Report per-cell, or one column matching the live config.

4. **A mechanism decomposition I built was misleading.** Bucketing the moved
   cells gave an apparent "+60.94R of re-entry suppression" upside. The buckets
   are cell-disjoint but **not setup-disjoint** — 14 of 19 moved setups appear in
   both a gain and a loss bucket — so there was never a clean set of good setups
   to keep. That is precisely why the scoped variant could not work.

5. **I wrongly called the 127 same-trade "worse fill" cells a systematic cost.**
   They are 62 negative / 65 positive, median **+0.03R**. The negative sum comes
   from a few large losses outweighing many small gains — a different claim.

## If this is revisited

Do **not** re-run this corpus; it is answered. n=9 distinct movers cannot
separate a −0.16R/setup bias from zero. The only thing that would add
information is **more pinbar entries on the live column** — a corpus-expansion
job. And even then, weigh the tail shape rather than the mean.

## What is on this branch

- `0da12a52` — half A (state machine pivot, 6 tests) + half B (`print_gate.rs`,
  the print-bar deferral, 7 tests). All 5 mutations caught.
- `44ff3dc3` — the re-entry-only variant (`Shot` enum, `PlanState::plain_entered`
  fire watermark, 11 tests). 6 mutations caught; note mutation (c), inverting
  first-vs-re-entry, was **invisible to the pure decision** and caught only at
  the engine entry point — mutate the entry point, not the layer below.
- `8a5c8211` — a sequence test that drives two pinbars through two ticks rather
  than seeding `plain_entered` by hand.

`all_fixtures_match_expected` is red on this branch **by design**: the 169/255
moved cells *are* the measurement, and re-blessing 2847 cells would destroy the
before/after evidence.

Baselines used (scratchpad, session-local): `full-before.json` (unchanged,
1071.5005783613474), `full-after.json` (full rule, 1025.4680952800363),
`variant.json` (re-entry only, 1037.2235677214258), plus `sanity-*.json`.
