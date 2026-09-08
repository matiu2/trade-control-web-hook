# Entry-rule comparison — re-run after the H4 aggregator fix

**Status:** RE-RUN 2026-09-08 against the corrected corpus (`b7e277b`).
**Verdict: unchanged — `skip-bcr` still wins, and by a slightly wider margin.**
**Method:** `scripts/compare-entry-rules.py` (paired per setup; no replay — reads
blessed `expected.json`).

This re-run was required because the previous conclusion was drawn on data
affected by `BUG-fixture-drops-monday-open-candle.md` /
`BUG-tn-h4-aggregator-emits-incomplete-bucket.md`.

## Headline (news=on, sl-anchor=signal — 61 paired setups)

| column | before | after | Δ |
|---|---:|---:|---:|
| **skip-bcr** (no break-and-close) | **+32.33** | **+34.30** | +1.97 |
| strategy-v2 (`--qm-entry market`) | +18.54 | +20.88 | +2.34 |
| strategy-v2 (`--qm-entry limit`, default) | +12.72 | +15.06 | +2.34 |
| normal (baseline H&S) | +6.60 | +6.69 | +0.09 |

`skip-bcr` also improves on the harder statistic: its **median** moves +0.04 →
**+0.26**, so the lead is not one outlier. Its W/L/0 goes 31/18/12 → 32/18/11.

**Robustness — `skip-bcr` wins all 6 (news × sl-anchor) slices, before and after:**

| slice | before | after |
|---|---:|---:|
| news=on, sl=signal | +32.33 | +34.30 |
| news=on, sl=invalidation | +20.19 | +18.76 |
| news=on, sl=fib-top | +14.71 | +13.38 |
| news=off, sl=signal | +34.86 | +36.83 |
| news=off, sl=invalidation | +21.43 | +20.00 |
| news=off, sl=fib-top | +16.84 | +15.52 |

## ⚠️ The corpus delta is TWO effects, and they point opposite ways

Corpus-wide Net R went **+309.62 → +297.63 (−11.99)**. Reading that as "the fix
cost 12R" is **wrong**. Re-arming to regenerate candles also rebuilt the plans
under a newer `tv-arm` (v132/v134 → v140), so two changes landed together.
Isolated by replaying the **old plans against the new candles**:

| | TN-H4 total R | isolates |
|---|---:|---|
| A. old plan + old candles (original) | +30.48 | — |
| B. old plan + **new candles** | **+49.34** | **the candle fix: +18.86** |
| C. new plan + new candles (committed) | +18.49 | the plan rebuild: **−30.85** |

**The H4 candle fix is strongly positive (+18.86R).** The headline decline is
entirely the tv-arm version bump on `eur-cad-h4-2026-07-23`, which is a
*separate* change that rode along with the re-arm.

Re-running the comparison on **B** (candle fix only, old plans throughout) gives
the same winner in all six slices, with `skip-bcr` at **+34.30** — so the verdict
does not depend on which of the two corpora you score.

## Biggest movers

| setup | before | after | Δ | cause |
|---|---:|---:|---:|---|
| `eur-cad-h4-2026-07-23` | +30.85 | +0.00 | **−30.85** | plan rebuild, **not** the candle fix |
| `gbp-nzd-h4-2026-08-05` | −1.00 | +17.84 | **+18.84** | the candle fix |
| `gbp-cad-h4-2026-08-07` | +6.63 | +6.65 | +0.02 | candle fix, immaterial |
| `nzd-usd-h4-2026-07-26` | −6.00 | −6.00 | 0.00 | unaffected |

Largest single cells:

| cell | before | after | Δ |
|---|---:|---:|---:|
| `gbp-nzd-h4-…-strategy-v2-qm-market-news-on` | +0.00 | +4.43 | +4.43 |
| `gbp-nzd-h4-…-strategy-v2-qm-market-news-off` | +0.00 | +4.43 | +4.43 |
| `gbp-nzd-h4-…-strategy-v2-news-off` | −1.00 | +2.25 | +3.25 |
| `eur-cad-h4-…-qm-market-news-on-sl-invalidation` | +2.93 | +0.00 | −2.93 |
| `eur-cad-h4-…-qm-market-news-off-sl-invalidation` | +2.93 | +0.00 | −2.93 |

**gbp-nzd** is the bug's headline case: it gains the operator-confirmed
`2026-08-09T21:00Z` week-open bar (O 2.29053 H 2.29364 L 2.28671 C 2.29249) and
converts a −1.00R stop-out into a take-profit worth up to +4.43R.

**eur-cad** now places no entry: under v140 its `too-low` is the drawn
invalidation `1.60867`, so golden signals at 1.603–1.606 are correctly rejected
as outside the SL..TP range.

## A real corpus defect this surfaced

In the **old** corpus, `eur-cad-h4-2026-07-23`'s `strategy-v2-qm-market` column
carried `too-low = 1.601239` while its three sibling columns carried `1.60867` —
even though `--save-matrix` exists precisely so every cell shares byte-identical
geometry. One setup in 62 was comparing entry rules across *different* plans.

The re-armed corpus is internally consistent: **0 of 62 setups** now disagree
across columns. That +30.85R the old corpus credited to `eur-cad` was partly an
artefact of the odd column, which is a second reason not to read the −11.99
headline as a loss.

## Reproduce

```sh
python3 scripts/compare-entry-rules.py                      # headline slice
python3 scripts/compare-entry-rules.py --all-slices         # 6-slice robustness
python3 scripts/compare-entry-rules.py -v --top 10          # per-setup detail
```

## Conclusion

**Keep `skip-bcr` as the entry rule.** It wins every slice in every corpus
variant tested, improves on both total and median R after the correction, and
its margin over `strategy-v2` is broad rather than outlier-driven: it differs on
31 of 61 setups and wins 18 of those 31 (against `strategy-v2`), so the lead is
carried by many setups rather than one or two.
