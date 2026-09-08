# BUG — `--entry-stop` doesn't recover to a limit, and replay can't see broker-side recovery

**Status:** OPEN, 2026-09-09. Two related defects found while measuring the
`--entry-matrix` axis. Operator's call on both: the rules **should be
symmetrical**, and a live↔replay divergence **is a bug**.

**Severity:** MEDIUM-HIGH. Neither moves a number in the current corpus (proven
below), which is exactly why they survived: the corpus cannot see either one.

---

## Bug 1 — the wrong-side recovery default is asymmetric

`tv-arm/src/hs_resolve.rs` (~line 422) picks the wrong-side recovery default:

| armed as | wrong-side default | should be |
|---|---|---|
| `--entry-limit` | recover to **stop** | (correct) |
| `--entry-stop` + `--require-confirmation` | recover to **limit** | (correct) |
| **`--entry-stop` alone** | **`Skip` — drops the entry** | **recover to limit** |

The engine is NOT the problem: `core/src/intent/resolution.rs` is already
symmetric — the Stop arm supports `Market` and `Limit` recovery, mirroring the
Limit arm's `Stop` recovery. Only the *default* is one-sided.

**Fix:** make `--entry-stop`'s wrong-side default `Limit`, unconditionally —
i.e. drop the `require_confirmation` condition so a stop recovers to a limit the
same way a limit recovers to a stop. `Skip` stays reachable via
`--recover-entry abort`.

### Why it changes nothing in today's corpus (and why that is NOT a reason to skip it)

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
signal bar. The asymmetry is a latent trap, not a live loss.

⚠️ Do not "fix" this by asserting the branch is dead and deleting it. The same
construction is what makes a **limit** wrong-side ~always, which is why
`-entry-limit` converges onto stops (601/102/86 identical order counts; 81.5%
of paired cells byte-identical).

## Bug 2 — replay cannot see broker-side recovery (`#19-10`)

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
