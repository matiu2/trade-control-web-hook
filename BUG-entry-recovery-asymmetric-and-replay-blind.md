# BUG — `--entry-stop` didn't recover to a limit, and replay can't see broker-side recovery

**Status:** Bug 1 **FIXED** 2026-09-09. Bug 2 **OPEN** and split out into its
own report — `BUG-replay-blind-to-broker-entry-recovery.md` — so it can be
worked independently; the summary below is retained for context.

Two related defects found while measuring the `--entry-matrix` axis.
Operator's call on both: the rules **should be symmetrical**, and a live↔replay
divergence **is a bug**.

**Severity:** MEDIUM-HIGH. Neither moves a number in the current corpus (proven
below), which is exactly why they survived: the corpus cannot see either one.
That is a statement about the corpus, **not** about the cost — and the cost
itself is contested (see the two conflicting sources below).

---

## Bug 1 — the wrong-side recovery default is asymmetric — FIXED

`tv-arm/src/hs_resolve.rs` (~line 422) picked the wrong-side recovery default:

| armed as | wrong-side default | should be |
|---|---|---|
| `--entry-limit` | recover to **stop** | (correct) |
| `--entry-stop` + `--require-confirmation` | recover to **limit** | (correct) |
| **`--entry-stop` alone** | **`Skip` — drops the entry** | **recover to limit** |

The engine is NOT the problem: `core/src/intent/resolution.rs` is already
symmetric — the Stop arm supports `Market` and `Limit` recovery, mirroring the
Limit arm's `Stop` recovery. Only the *default* is one-sided.

**Fix — SHIPPED.** The default is now keyed off the entry order type alone, in
one expression, with `--recover-entry` overriding it:

```rust
recover_entry: args.recover_entry.map(|r| r.into_core()).unwrap_or(
    match args.pattern_entry_mode() {
        Some(PatternEntry::Market)      => RecoverEntryAction::Skip,
        Some(PatternEntry::Limit)       => RecoverEntryAction::Stop,
        Some(PatternEntry::Stop) | None => RecoverEntryAction::Limit,
    },
),
```

This is deliberately the **same rule the QM leg already applied** —
`match spec.qm_entry_mode` in `cli/src/trade_patterns.rs` — which was itself
written from the operator's reasoning ("a Stop recovers to a Limit … a Limit
recovers to a Stop … Market has no resting order to recover"). The BCR leg was
the odd one out, not the QM leg. `require_confirmation` no longer participates:
it governs *when* an entry may fire, not what happens when it lands wrong-side.
`Skip` stays reachable via `--recover-entry abort`.

**M/W keeps `Skip` — WON'T FIX, deliberately.** `tv-arm/src/mw_resolve.rs`
still hard-codes `RecoverEntryAction::Skip`, and that is now a recorded decision
rather than an untouched corner (operator, 2026-09-09):

> An M/W trade moves much faster than an H&S, and its edge is taking the
> reversal **at the top, on the way down** (mirror for a W). If the entry is
> missed and price later comes back to tag a resting limit, that fill is no
> longer the setup — it's a late entry into a move that already ran. Get in at
> the top or not at all.

So the stop→limit recovery that is right for an H&S is *wrong* for M/W — this is
a behavioural difference between the patterns, not an inconsistency to be tidied
away. M/W is also not currently traded (it has not been profitable in practice),
so there is no live exposure either way.

`BUG-stop-entry-recover-defaults-to-skip.md` item 2 asks for M/W to at least
honour `args.recover_entry`; that is declined, and would not work as described
anyway — the M/W enter builder takes an `MwSpec` and **never constructs an
`EntrySpec::Stop`** (the worker resolves M/W geometry from `intent.mw` at fill),
so there is no field for a recovery to ride on and the flag would bake a value
nothing reads. The rationale lives at the `recover_entry` line in
`mw_resolve.rs` so it isn't "helpfully" wired up later.

`Args::limit_recover_action` was **deleted**, not left in place. It was a
second, limit-only derivation of the same rule, and keeping it would have left
two places that must agree by hand — the shape that produced this bug. Its unit
test went with it, replaced by tests at the layer that matters (below).

### Verification

`cargo test --workspace` green, including the 304-test CLI suite that scores
the full 2695-cell corpus — so the corpus is byte-unchanged, as predicted.

Because green tests prove nothing here, the three new tests were
**mutation-tested at the entry point** (`hs_resolve`, which returns the real
`TradeSpec` that gets signed) rather than on an `Args` helper — the layer the
old test sat at, and the reason the asymmetry survived:

| mutation | caught by |
|---|---|
| stop arm `Limit` → `Skip` (restore the original bug) | both symmetry tests |
| limit arm `Stop` → `Limit` (break the mirror) | `wrong_side_recovery_is_symmetric_across_entry_types` |
| market arm `Skip` → `Limit` (recover a non-resting order) | same |
| ignore an explicit `--recover-entry` | `explicit_recover_entry_overrides_the_default_on_every_entry_type` |

No survivors.

### Did it cost live trades? Two sources disagree — `BUG-stop-entry-recover-defaults-to-skip.md`

That report (from demo-journal trade 154, USD/ZAR H1, plan `hs-usd-zar-1ac62120`)
independently found this same default and reached the same fix (`Limit`, for the
same reason). Its evidence:

| trade | instrument | live | as-designed replay |
|---|---|---|---|
| 049 | GBP/NZD | no entry (`too-close`) | trade ran |
| 050 | — | no entry (`too-close`) | trade ran |
| 154 | USD/ZAR H1 | no entry (`too-close`) | −1.00R |

In each, `05-enter` fired once, the broker rejected it `#19-10`, and the plan sat
inert until expiry — no veto, no decline, nothing in the outcome ledger. Trade
154's replay happened to lose, so the drop *saved* 1R by luck, not by design.

⚠️ **A concurrent database sweep contradicts this and is not dismissible either.**
Zero `entry-failed: too-close-to-market` outcomes across staging (19,872 rows,
2026-07-06 → 09-08) and dev — and plan `hs-usd-zar-1ac62120` is **absent from
both databases** despite the window covering it (present USD/ZAR ids are
`2802f414`, `ac81ae99`, `25b08c7f`, `b042053f`). Verified independently here.

Points in each direction, so that whoever resolves this starts from facts:

- **For the incident being real:** the USD/ZAR plan was armed on **TradeNation**
  (checked: `plan.json` carries `broker: tradenation`), the only broker that
  emits `#19-10` — `broker-oanda` never constructs the variant. A stale or
  re-armed plan id is a known trap in this repo (cf. trade-142, whose bug doc
  named a re-armed copy).
- **For the zero being real:** the sweep anchored on the exact outcome string
  rather than a `LIKE '%too-close%'`, which returns 15 false rows because the
  worker records each request *body* into the next row's outcome.
- **Weakening both:** the journal keeps only ~7 days of detail, so absence is
  weak evidence, and the incident doc's own frequency table is unsourced.

**Treat the cost as unresolved.** Don't quote either number as settled.

Two consequences worth carrying forward:

- **A `Skip` biases the ledger.** A forfeited trade leaves no row whether it
  would have won or lost, so the outcome distribution of dropped entries is
  currently unmeasurable.
- **The claimed forfeits all came through the BROKER route** (`#19-10`), not the
  resolve-time one. That is exactly the route replay cannot see — Bug 2 — which
  is why the byte-identical corpus result below must be read narrowly, whichever
  way the frequency question lands.

### Why it changes nothing in today's corpus — and why that proves LESS than it looks

⚠️ The measurement below covers the **resolve-time** branch only. The three
forfeits above went through the **broker** branch, which replay never exercises
at all. A green corpus was never evidence that this default was harmless.

Measured 2026-09-09: re-arming with `--entry-stop --recover-entry limit`
(verified it bakes `{"action":"limit"}` onto the plan) is **byte-identical on
13/13 setups**. The branch is unreachable *for the current geometry*:

> a long stop triggers at `signal_high + 0.5%·ATR`, i.e. ABOVE the signal bar's
> own high, and resolution runs on that same bar whose close cannot exceed its
> high — so a stop is correct-side **by construction**.

That is a property of `from: signal_high` + a positive offset resolved on the
signal bar. Anything that breaks it makes the branch live: a multi-bar
confirmation wait, a zero/negative offset, an absolute `at`, an M/W leg with
different anchoring, or a future entry anchored off something other than the
signal bar.

⚠️ Do not "fix" this by asserting the branch is dead and deleting it. The same
construction is what makes a **limit** wrong-side ~always, which is why
`-entry-limit` converges onto stops (601/102/86 identical order counts; 81.5%
of paired cells byte-identical).

## Bug 2 — replay cannot see broker-side recovery (`#19-10`) — OPEN

> **Now tracked in `BUG-replay-blind-to-broker-entry-recovery.md`**, which adds
> the evidence gathered since: 1339 of 2695 corpus cells configure a
> `recover_entry` replay can never execute, and a probable third defect —
> `place_entry_too_close_fallback` admits only `ResolvedEntry::Stop`, making
> `RecoverEntryPlan::Stop` unreachable from its only caller, which is exactly
> what the 888 `entry-limit` cells configure. The summary below is retained for
> context; work from the split-out report.

There are **two** recovery routes. Replay implements one:

| route | where | in replay? |
|---|---|---|
| resolve-time wrong-side | `core/src/intent/resolution.rs` | **yes** — shared by worker + replay |
| broker rejection `#19-10` | `core/src/recover_entry.rs` | **NO** |

`recover_entry.rs` handles TradeNation's `EntryTooCloseToMarket` and re-places
as market or limit. The replay broker
(`cli/src/bin/replay_candles/replay_broker.rs`) **never raises that error**, so
the path is dead offline: **0 recoveries across all 2695 corpus cells**, and
`expected.json` has no field that would record one (a leg carries
`entry_time/entry_price/stop_loss/take_profit/exit_*/r` — no order type, no
recovery marker).

Consequences:
- A live stop entry can be re-placed as a limit/market and fill at a price the
  replay never models — silent live↔replay divergence on exactly the entry type
  the strategy ships with.
- The journal's as-designed R for any trade that hit `#19-10` is unsound.

**Fix sketch (not built):** give the replay broker a rejection model for
"trigger too close to / already past market" so `run_enter`'s recovery path is
exercised offline, and record the recovery on the leg (order type + whether it
was recovered) so a fixture can pin it. Both halves are needed: without the
recorded field the fixture still can't tell the two apart.

## Repro

```sh
# Bug 1 — identical results prove the branch is unreachable today
tv-arm-staging --spec-in replay-fixtures/eur-cad-h4-2026-07-23.spec.json \
  --skip-bcr --entry-stop --recover-entry limit \
  replay --save probe --simulate true --instrument eur/cad --fixtures-dir /tmp/probe
# → same Net R as replay-fixtures/eur-cad-h4-2026-07-23-skip-bcr-news-on-entry-stop

# Bug 2 — nothing in the corpus records a recovery
grep -rl "recover\|too-close" replay-fixtures/*/expected.json | wc -l   # → 0
grep -rn "EntryTooCloseToMarket" cli/src/bin/replay_candles/            # → no hits
```

## Context

Found while adding `--entry-matrix` (v?; `8cd1aad`) and regenerating the corpus
to 2664 grid cells (`74ba77b`). The measured entry-order-type result is in
`entry-rule-corpus-comparison.md`: market is worse than stop against all four
entry rules; limit ≈ stop because limits convert to stops.
