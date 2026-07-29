# TODO — invalidation-close mislabel + post-exit journal events

Found via GBP/NZD iH&S 2026-07-22 replay (`tv-arm-staging replay`):

```
2026-07-23 15:00  entry #1 CLOSED AT EXPIRY → 2.29996  (R: -0.25)   ← actually 01-veto-too-low
2026-07-23 15:00  Veto (01-veto-too-low) — no fill simulated
2026-07-23 21:00  entry #1 SL→break-even (a candle closed past 50%-to-TP)   ← position already closed
2026-07-24 06:30  entry #1 SL widened → 2.28817 (spread blackout System 2…) ← ditto
2026-07-24 18:00  entry #1 SL restored → 2.29315                            ← ditto
```

The trade-expiry is 2026-07-27; nothing expired.

## Bug A — every `ClosePositions` veto is labelled "AT EXPIRY"

`replay.rs::516` hardcodes `ExitReason::Expiry` for the whole
`Action::Veto | Action::Invalidate if level == ClosePositions` arm. Both the
trade-expiry veto AND the structure-invalidation veto (`too-low` for a long /
`too-high` for a short) are `ClosePositions`, so an invalidation close prints
"CLOSED AT EXPIRY", tallies as `EXP:`, and annotates the chart `outcome="expiry"`.

For an iH&S **long**, `too-low` IS the invalidation veto and closing is correct
(`trade_patterns.rs:1104`, `VetoLevel::ClosePositions`) — only the *label* is
wrong. Not a recurrence of `BUG-too-low-closes-positions.md` (that was the
*short* case, where `too-low` is pcl-exhausted → `StopNextEntry`).

- [x] A1. New `ExitReason::Invalidation` (broker + economics) and
      `FillKind::ClosedOnInvalidation`, threaded from the fire's rule so the
      arm distinguishes trade-expiry from invalidation instead of guessing.
- [x] A2. Journal line: `CLOSED ON INVALIDATION (<rule>) → …`; chart label
      `invalidation`; tally `INV:` separate from `EXP:`.
- [x] A3. `baseline.rs` shape tuple + `BaselineEntry` carry `invalidation_closes`
      so a same-R change of character is still detected.

## Bug B — stop-management events narrated after the position closed

`report.rs::entry_events` emits SL→break-even and the spread widen/restore lines
**before** resolving the exit (`resolve_fire_any` comes later), and both helpers
(`fill_sim::breakeven_armed_at_resolved`, `widened_stop_at_resolved`) walk the
whole `fire.forward` window stopping only at SL/TP — they don't know about a
broker-side flatten (reversal-close or a `ClosePositions` veto).

So a position flattened at 15:00 still narrates BE arming at 21:00 and a widen
the next day, reading as though it were open. Cosmetic (the ledger's R is
correct) but it makes the timeline lie.

- [x] B1. Resolve the outcome FIRST in `entry_events`, then bound the forward
      window at the realized exit bar before the display helpers walk it.
- [x] B2. Regression test: a position flattened by an invalidation veto emits no
      post-exit SL/BE/widen lines.

## Verification

- [x] uk-100 fixture keeps `exit_reason: "expiry"` (genuine `02-veto-trade-expiry`)
- [x] 6 × gbp-nzd-h1-2026-07-22-* fixtures flip to `invalidation` (re-blessed)
- [x] mutation-verify both fixes (per `[[verify_new_analysis_code_by_mutation]]`)
- [x] cargo clippy + fmt

Mutation results (each reverted after):

| mutation | caught by |
|---|---|
| `is_expiry = true` (old level-keyed classify) | 6/6 gbp-nzd `--check` fail; `an_invalidation_close_…` fails on the counter |
| `lifetime = &fire.forward` (unbounded window) | `an_invalidation_close_…` names the 3 post-exit lines |

Note the `expected.json` golden **cannot** catch Bug B on its own — it records
legs, not journal text, and the mislabelled run booked an identical −0.25R leg.
That's why the text assertion in `an_invalidation_close_is_labelled_and_ends_the_journal`
is load-bearing, not decorative.

## Found along the way — SEPARATE pre-existing bug, not fixed here

`replay-candles --test-mode --fixtures-glob '*' --check` reports **21 ok / 10
failed**, and that count is **identical with this branch's changes stashed** —
so it predates this work.

Every failure is a 1-ULP float difference on a leg's `stop_loss` from the
SL-spread floor, e.g. `2.29315` (loaded from `expected.json`) vs
`2.2931500000000002` (recomputed). The replay itself is deterministic — three
consecutive runs give the same bits — and a `--rebless` immediately followed by
`--check` still fails, so save and load+compare genuinely disagree on the value.

Two consequences worth knowing:

1. **`all_fixtures_match_expected` hides it.** The test `panic!`s on the FIRST
   mismatch, and `eur-usd-h1-2026-07-22-skip-bcr-news-off` sorts 12th of 31 —
   so it never reaches the gbp-nzd fixtures at 18+. The in-repo test looks like
   a single failure; the CLI shows all ten. A per-fixture soft-fail (collect,
   then assert) would surface the real blast radius.
2. **The `--check` diff output is misleading.** `diff_error` pretty-prints both
   sides via `serde_json`, whose shortest-round-trip float formatter renders the
   two 1-ULP-apart values as `2.29315` and `2.2931500000000002` — but a file
   holding `...0002` can still *load* as the other. Reading the printed diff
   alone sends you looking for a rounding step that isn't there.

Worth its own task: either make the floor bit-stable, or compare legs with a
tick-scaled epsilon instead of exact float equality.
