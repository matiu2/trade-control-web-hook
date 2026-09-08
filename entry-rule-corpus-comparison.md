# Entry-rule comparison — re-run after the H4 aggregator fix

**Status:** RE-RUN 2026-09-08 against the corrected corpus (`b7e277b`), then
**revised the same day** after the operator corrected a mis-drawn `too-low`
invalidation line on `eur-cad-h4-2026-07-23` (see "Correction" below).
**Verdict: unchanged — `skip-bcr` still wins, and by a wider margin (+32.33 → +40.91).**
**Method:** `scripts/compare-entry-rules.py` (paired per setup; no replay — reads
blessed `expected.json`).

This re-run was required because the previous conclusion was drawn on data
affected by `BUG-fixture-drops-monday-open-candle.md` /
`BUG-tn-h4-aggregator-emits-incomplete-bucket.md`.

## Headline (news=on, sl-anchor=signal — 61 paired setups)

| column | before | after | Δ |
|---|---:|---:|---:|
| **skip-bcr** (no break-and-close) | **+32.33** | **+40.91** | +8.58 |
| strategy-v2 (`--qm-entry market`) | +18.54 | +23.07 | +4.53 |
| strategy-v2 (`--qm-entry limit`, default) | +12.72 | +20.21 | +7.49 |
| normal (baseline H&S) | +6.60 | +6.69 | +0.09 |

`skip-bcr` also improves on the harder statistic: its **median** moves +0.04 →
**+0.27**, so the lead is not one outlier. Its W/L/0 goes 31/18/12 → 33/18/10.

**Robustness — `skip-bcr` wins all 6 (news × sl-anchor) slices, before and after:**

| slice | before | after |
|---|---:|---:|
| news=on, sl=signal | +32.33 | +40.91 |
| news=on, sl=invalidation | +20.19 | +20.19 |
| news=on, sl=fib-top | +14.71 | +14.71 |
| news=off, sl=signal | +34.86 | +43.43 |
| news=off, sl=invalidation | +21.43 | +21.43 |
| news=off, sl=fib-top | +16.84 | +16.84 |

## Correction — a mis-drawn `too-low` line, now fixed

An earlier revision of this document reported the corpus getting *less*
profitable (+309.62 → +297.63) and blamed a tv-arm version bump. **That was
wrong**, and the cause was a bad drawing, not the code.

`eur-cad-h4-2026-07-23`'s frozen spec carried `invalidation = 1.60867`. This is
an **iH&S long**, whose `too-low` is the invalidation *floor*: price falling back
below the head kills the setup, so the line must sit at or below the head
(**1.601**). At 1.60867 it sat **above** the head — *inside* the trade — so it
vetoed the setup's own entries. Every `eur-cad` cell scored +0.00R with three
golden signals rejected `veto-active (too-low)`.

The operator redrew the line and re-armed; the spec now carries
`invalidation = 1.6012386`, just below the head. Regenerating all 24 cells from
the corrected spec takes `eur-cad-h4-2026-07-23` from **+0.00 → +54.35R**.

**The corpus is therefore MORE profitable after the fixes, not less:**

| | before | after |
|---|---:|---:|
| TN-H4 subtotal | +30.48 | **+72.85** |
| whole corpus | +309.62 | **+351.99** |

## Biggest movers (final)

| setup | before | after | Δ | cause |
|---|---:|---:|---:|---|
| `eur-cad-h4-2026-07-23` | +30.85 | +54.35 | **+23.50** | corrected `too-low` line |
| `gbp-nzd-h4-2026-08-05` | −1.00 | +17.84 | **+18.84** | the H4 candle fix |
| `gbp-cad-h4-2026-08-07` | +6.63 | +6.65 | +0.02 | candle fix, immaterial |
| `nzd-usd-h4-2026-07-26` | −6.00 | −6.00 | 0.00 | unaffected |

**gbp-nzd** is the candle bug's headline case: it gains the operator-confirmed
`2026-08-09T21:00Z` week-open bar (O 2.29053 H 2.29364 L 2.28671 C 2.29249) and
converts a −1.00R stop-out into a take-profit worth up to +4.43R.

### Cross-check against an independent operator replay

The operator replayed the corrected setup with `tv-arm-staging --skip-bcr
--entry-market` and got **+8.51R**. The corpus `skip-bcr-news-on` cell scores
**+6.61R** on the same three trades — same entries, same reversal-close, same
stop-out, same take-profit. The gap is entirely the entry type: `--entry-market`
fills at the signal-bar close, the corpus default rests a **stop** at the
geometry anchor and fills a bar later at a worse price. On a long that shrinks
every R:

| | market fill | stop fill |
|---|---:|---:|
| entry #1 | 1.60178 | 1.60321 |
| entry #2 | 1.60589 | 1.60637 |
| entry #3 | 1.60408 | 1.60482 |

Note `--entry-market` is a **fifth** entry configuration; the corpus grid covers
normal / skip-bcr / strategy-v2 / strategy-v2-qm-market only. If it is to be
compared properly it needs its own column, not a one-off replay.

## Reproduce

```sh
python3 scripts/compare-entry-rules.py                      # headline slice
python3 scripts/compare-entry-rules.py --all-slices         # 6-slice robustness
python3 scripts/compare-entry-rules.py -v --top 10          # per-setup detail
```

## Conclusion

**Keep `skip-bcr` as the entry rule.** It wins every slice in every corpus
variant tested, improves on both total and median R after the correction, and
its margin over `strategy-v2` is broad rather than outlier-driven — the lead is
carried by many setups, not one or two.

Two caveats worth carrying forward:

1. **`--entry-market` is untested as a column.** The one setup measured suggests
   a market fill beats the stop entry on a long (+8.51 vs +6.61R) simply by
   filling earlier. That is one setup, one direction — it may well reverse on a
   short, where filling earlier means filling worse. It deserves a real column
   before anyone reads anything into it.
2. **Drawings are an input, and a bad one is invisible here.** The `eur-cad`
   `too-low` line silently zeroed a whole setup and nothing in the corpus
   flagged it. A geometry sanity check — for an iH&S, assert `too-low ≤ head`;
   for an H&S, `too-high ≥ head` — would have caught it at arm time.

## Order type (market / stop / limit) — what the corpus can and cannot answer

**Asked:** do we have pivots for `--market-entry` / `--stop-entry` /
`--limit-entry`, and is market more profitable?

**No, not as a column.** The grid's four columns vary the *entry rule*, not the
*order type*, and `meta.arm` does not record the order type at all. What the
plans actually contain:

| column | main `enter` leg | QM second leg |
|---|---|---|
| normal | **stop** | — |
| skip-bcr | **stop** | — |
| strategy-v2 | **stop** | **limit** |
| strategy-v2-qm-market | **stop** | **market** |

So **every main entry in the corpus is a stop** (911 of 911 cells). There is no
market-vs-stop pivot for the primary entry anywhere in the corpus.

⚠️ Don't confuse the two flag families: `--entry-market` / `--entry-stop` /
`--entry-limit` are the **pattern path** (H&S / M&W geometry). `--market-entry` /
`--stop-entry` / `--limit-entry` are the **position-tool path** — they read a
drawn position, place immediately, and carry no plan, preps, vetos or geometry
(and no `EntryAttempt` row, so nothing manages them). The position-tool family is
not comparable to a corpus column at all.

### The one order-type pivot that DOES exist, and it is not conclusive

`strategy-v2` vs `strategy-v2-qm-market` differ *only* in the QM leg's order type
(limit vs market), so it is a clean paired test — on the second leg only.

Market wins on total R in **all 6 slices** (headline slice +23.07 vs +20.21), and
on that evidence alone the "market is better" theory looks supported. It does not
survive a closer look:

| statistic | value |
|---|---|
| paired setups (news=on, sl=signal) | 62 |
| setups where the two differ | 24 |
| market better / worse | **9 / 15** |
| sum Δ (market − limit) | +2.86 |
| **median Δ of differing setups** | **−0.05** |
| sign test | **p = 0.31** |
| sum Δ excluding the 2 largest outliers | **−5.87** |

Market **loses on more setups than it wins** and its median is negative; the
positive total is carried by two outliers (`aud-cad-h1-2026-07-28` +4.50,
`nzd-jpy-h1-2026-08-05` +4.23). Splitting by direction does not rescue it —
market's median is negative for longs (−0.03, 4 better / 4 worse) *and* shorts
(−0.06, 5 better / 11 worse).

Nor is it a participation effect: limit fills 72 legs across those setups, market
70, so this is fill *price* on much the same trades, not trading more often.

**Verdict: unproven, not disproven.** A one-setup observation (`eur-cad-h4`,
`--entry-market` +8.51R vs the corpus stop's +6.61R) plus a total-R win in every
slice is a genuinely reasonable prior — a market order fills at the signal close
while a stop waits for the break and fills worse, which on a long is strictly
better *when the trade works*. The corpus just cannot confirm it, because the
only order-type pivot it holds is on the QM leg and that pivot is
outlier-driven and insignificant.

### What would actually answer it

A fifth column — `skip-bcr` with `--entry-market` on the **main** leg — armed
across the corpus from the frozen specs. `skip-bcr` is the shipped rule, so that
is the comparison that matters, and it needs no new chart reads:

```sh
tv-arm --spec-in <setup>.spec.json --skip-bcr --entry-market \
  replay --save <setup>-skip-bcr-market-news-on --instrument <inst>
```

The paired-comparison machinery already handles a new column; `parse_cell_name`
and `COLUMNS` in `scripts/compare-entry-rules.py` need the label adding.
