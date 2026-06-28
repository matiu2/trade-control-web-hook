# BUG: replay-candles never evaluates the golden-candle gate — entries fill on non-golden candles despite `needs_golden: true`

**Status:** 🟥 OPEN — found 2026-06-28 while building the trade 075 Wheat entry-style comparison.
**Component:** two defects, one per side of the pipeline:
1. `tv-arm-staging` — `--skip-golden` does **not** clear the gate (plan still carries `needs_golden: true`).
2. `replay-candles-staging` — the golden gate is **never evaluated** per-tick; entries fill regardless of golden status.
**Severity:** correctness — the golden gate is documented as *used on every trade, always*
([[project_entry_rule_tags]]). If the replay ignores it, **every as-designed replay outcome on
every page may be wrong**: entries fire on the first stop-trigger instead of waiting for a
qualifying golden candle (≥1 ATR), changing fill price, timing, and R.

---

## One-line summary

`grep -ci golden` over a full `RUST_LOG=debug` replay = **0**. The engine never mentions
golden. Yet the plan carries `needs_golden: true`. So the gate is loaded but has no per-tick
code path — an order fills on the first stop-trigger even when the entry candle is not golden.

---

## Concrete case — trade 075 Wheat, `raw` style (extended-expiry what-if)

The `raw` plan was armed `--skip-break-and-close --skip-retest --skip-golden`. Two things
went wrong:

1. **`--skip-golden` ignored.** The plan JSON still has `needs_golden: true` (so "raw" wasn't
   actually golden-free).
2. **Gate not enforced anyway.** The order fired on the **06-23 11:00** bar close and filled
   on **06-23 12:00 @ 5.9818** — and the **12:00 candle is not a golden candle** (operator
   confirmed on the chart; Wheat ATR ≈ 0.065, so golden needs a ~0.065 range/body, which that
   bar doesn't have). The fill should have been blocked or deferred to the next golden bar.

The two defects cancelled out to make `raw` look like a clean **+1R TP** win — but the entry
is illegitimate as-designed: with the golden gate enforced, the fill bar (and therefore the
entry price, the BE timing, and possibly the outcome) would differ.

---

## Engine evidence

```
# arm side — skip-golden ignored:
tv-arm-staging --plan-out wheat-raw.json --skip-break-and-close --skip-retest --skip-golden
python: wheat-raw.json  ->  needs_golden = True        # should be False

# all five styles carry the gate:
wheat-075 (BCR):  needs_golden True, True
wheat-raw (raw):  needs_golden True                    # despite --skip-golden
wheat-qm:         needs_golden True, True
wheat-v2:         needs_golden True, True, True
wheat-qmm:        needs_golden True, True

# replay side — gate never evaluated:
RUST_LOG=debug replay-candles-staging --plan wheat-raw.json --instrument WHEAT_USD --source oanda \
  | grep -ci golden        # -> 0   (no golden evaluation anywhere)
# order fills 06-23 12:00 @ 5.9818 on a NON-golden candle.
```

---

## Root cause (hypothesis)

Same class as the already-found `06-close-on-reversal` and `pause-not-enforced` gaps: the rule
/ gate is present in the plan and honoured by the live worker, but the replay's per-tick loop
has **no handler** for it. The golden check is presumably wired into the live worker's entry
path but was never ported to the simulator, so `needs_golden` is inert in replay.

The `--skip-golden` arm defect is separate: the flag is parsed but doesn't flip
`needs_golden` to `false` on the emitted enter intent(s).

---

## Reproduction

```bash
export OANDA_TOKEN=… OANDA_ACCOUNT_ID=101-011-31142393-003
# arm raw — note --skip-golden:
tv-arm-staging --plan-out /tmp/raw.json --skip-break-and-close --skip-retest --skip-golden
python3 -c "import json;d=json.load(open('/tmp/raw.json'));print('needs_golden still set:', 'needs_golden\": true' in json.dumps(d))"   # True == bug 1
RUST_LOG=debug replay-candles-staging --plan /tmp/raw.json --instrument WHEAT_USD --source oanda 2>&1 | grep -ci golden   # 0 == bug 2
```

---

## Suggested fix

1. **Replay side — add a golden evaluation to the per-tick entry path.** Before filling a
   stop/limit enter whose intent has `needs_golden: true`, test the candidate entry candle for
   golden (range/body ≥ 1 ATR, the same definition the live worker uses) and **defer the fill
   to the first qualifying bar** (or decline per the live semantics). Log it (`golden: ok @ …`
   / `golden: blocked @ … (range 0.0xx < ATR 0.065)`) so it's greppable — same treatment the
   new `be:` event got.
2. **Share the golden test** between worker and replay via `trade_control_core` so they can't
   diverge.
3. **Arm side — make `--skip-golden` actually clear `needs_golden`** on every emitted enter
   intent (BCR stop, QM limit, v2's sibling, etc.).
4. Regression tests: (a) a plan with `needs_golden: true` must NOT fill on a sub-ATR candle;
   (b) `--skip-golden` must emit `needs_golden: false`.

## Verification after fix

Re-replay trade 075 all five styles. With the gate enforced, entries should only fill on
golden candles; the `raw` +1R may change fill bar/price or outcome. `grep -ci golden` > 0 in
the debug log. `--skip-golden` plans show `needs_golden: false`.

Related: `BUG-replay-close-on-reversal-not-evaluated.md`,
`BUG-replay-candles-pause-not-enforced.md` (sibling, FIXED), [[project_entry_rule_tags]]
(golden documented as always-on). Trade page: `src/trade-075-wheat-hs.md`.
