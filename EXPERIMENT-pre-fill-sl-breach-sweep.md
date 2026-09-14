# Experiment — is the pre-fill SL-breach sweep worth keeping?

**Date:** 2026-09-14 · **Corpus:** 2847 cells, 1600 taken · **Baseline Net R:** +1053.1756

## The question

When a stop-entry order is resting and price runs against us through the
stop-loss *before* the order triggers, we cancel the order. Two open questions:

1. Do we need the rule at all?
2. Must price **wick** past the SL, or **close** past it?

Live and replay genuinely disagree on (2): the live cron calls
`get_current_price` every tick (effectively **wick**-sensitive); the offline
replay and a close-only staging tick sample one point per bar (**close**).

## How it was made measurable

`TC_REPLAY_SWEEP_BREACH=close|wick|off` in
`cli/src/bin/replay_candles/fill_sim.rs::truncate_at_pre_fill_sl_breach`.

- `close` (default) — bar mid close. Historical behaviour.
- `wick`  — bar adverse extreme (long → low, short → high). Live's analogue.
- `off`   — no sweep; a breached resting order stays live and may fill.

Env var rather than a CLI flag because `find_fill` is reached from three public
entry points that carry no replay settings. An unrecognised value **panics**
rather than silently defaulting — a typo that quietly measured the baseline
twice would produce a confident, wrong conclusion.

## Results

| mode | Net R | vs baseline | cells moved |
|---|---|---|---|
| `close` (default) | +1053.1756 | — | — |
| `off` | +1053.1756 | **0.0000** | **0** |
| `wick` | +1071.5006 | **+18.3250** | 14 |

Default and explicit `close` both reproduce the blessed corpus exactly, and the
full corpus stays green (`--check` 2847/2847) in every arm.

## Finding 1 — `off` is identical, but NOT because the rule is inert

The tempting read is "the sweep never fires, so delete it". That is **wrong**.
Instrumenting the truncation shows it fires constantly:

| mode | truncations | of which actually cut bars |
|---|---|---|
| `close` | 1600 | **854** |
| `wick` | 4538 | **2602** |

So in close mode the rule cuts the fill window on 854 orders — and changes the
outcome of **none** of them. Disabling it creates **0** newly-taken cells
(1600 taken in both arms).

**Why:** the sweep only matters when a breached order would *later* have come
back through its trigger and filled. In this corpus that essentially never
happens — once price runs through the stop of a resting entry, it does not
return to trigger it within the alert window. The rule is a **no-op on
outcomes here, not a no-op in mechanism.**

That distinction matters: it means the corpus shows **no evidence the rule
earns anything**, but also **no evidence it costs anything**. It is not
proof the rule is unnecessary in live — see the caveat below.

## Finding 2 — `wick` is better, but it rests on ONE setup

+18.33 R, 14 cells, 0 cells worse. That looks decisive and is not:

- All 14 moved cells are grid variants (entry × news × sl-anchor) of a
  **single chart setup**, `eur-cad-h4-2026-07-23` (iH&S long).
- That is **1 event**, not 14 independent observations. 0.88% of taken cells.
- Mechanism (verified per-cell): under `wick` a **second, losing re-entry is
  suppressed** — `sl_hits` 1 → 0 — so the multi-shot trade keeps its winner
  and drops a re-entry that was already proven wrong. The exits go
  `tp=1, sl=1` → `tp=1, sl=0, reversal=1`.

The mechanism is sound and points the right way. The sample does not support a
number. **+18.33 R is one setup's worth of evidence, not a corpus-wide edge.**

## Caveat — the corpus cannot settle this

The goldens were recorded under close-sampling, so by construction the corpus
contains almost no case where a bar wicked past the stop and closed back
inside. The `wick` arm found only one such setup. This is the same structural
blindness noted for timing-sensitive gates: **no fixture is evidence about a
sampling rule the fixtures were generated under.**

Also note replay ≠ live here regardless of mode: live samples a quote per tick,
so it sees excursions no bar-grain replay can reconstruct. `wick` is the
closest offline *approximation* of live, not a reproduction of it.

## Recommendation

1. **Do not delete the rule on this evidence.** `off` costs nothing in the
   corpus, but the corpus cannot see the case the rule exists for (a breached
   order that later fills), and live's per-tick sampling reaches states replay
   cannot. Deleting it trades a real live protection for a measured-zero
   offline gain.
2. **`wick` is the better-aligned default** — it matches live's sampling and,
   where it acted, it suppressed a losing re-entry. But ship it as a *parity*
   fix (replay matching live), not as a claimed +18 R edge.
3. **To actually settle it**, the corpus needs setups selected *for* the wick
   case: resting entries whose stop was breached intrabar and which later
   returned to trigger. Until then this is one observation.

## Reproduce

```sh
cargo build --release -p trade-control-cli --bin replay-candles
for M in close wick off; do
  TC_REPLAY_SWEEP_BREACH=$M ./target/release/replay-candles \
    --test-mode --fixtures-glob '*' --fixtures-dir replay-fixtures --json \
    --bless-baseline /tmp/bl-$M.json > /tmp/m-$M.json 2>/dev/null
done
```

---

## Outcome — what shipped (2026-09-14)

Recommendation 2 was taken, and extended: **both sides** were moved onto the
same question, "has price TRADED past the stop since placement".

- **Replay** reads each bar's **adverse extreme** (low for a Long, high for a
  Short) instead of its mid close — the `wick` arm above, made the behaviour
  rather than an env var. `TC_REPLAY_SWEEP_BREACH` is not shipped; there is one
  behaviour, not a switch.
- **Live** was the deeper half of the divergence, and the experiment did not
  measure it. It read `Broker::get_current_price` — an *instantaneous* spot
  quote on the ~900s upkeep loop — so its answer depended on where the cron tick
  landed on the price path, and any excursion between two ticks was invisible.
  It now folds each tick's quote into a **running adverse extreme** persisted on
  the `EntryAttempt` row (`adverse_extreme`) and judges the breach against that.
  The decision is monotonic: once the extreme is past the stop it stays past.

So the two sides now differ only in *resolution* (per-tick vs per-bar), not in
the question they ask. `wick` was never "live's analogue" in the strict sense
the experiment claimed — live-before was a sampling lottery, not a wick rule —
which is why the live half was changed too rather than replay alone being bent
to match it.

The caveats above stand unchanged and are reproduced at the code:
`core::sweep_gate::update_adverse_extreme` and
`fill_sim::truncate_at_pre_fill_sl_breach` both carry the "854 truncations, zero
outcomes changed, and the corpus cannot see the case the rule exists for"
warning, so a future reader does not simplify the rule away on green fixtures.
