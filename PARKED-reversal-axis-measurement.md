# PARKED — is the reversal-close worth keeping?

**Status:** MEASURED, INCONCLUSIVE, 2026-09-16. The tooling ships; the standing
corpus cost does not. `--skip-reversals` is available for one-off replays;
`--reversal-matrix` exists but is **off by default**.

---

## The question

A reversal-close flattens an open position when a golden opposing candle prints
during a news window (`06-close-on-reversal`) or off a support/resistance band
(`07-close-on-sr-reversal`). It is non-terminal — it closes the position but
never blocks re-entry (see CLAUDE.md, "CLOSE vs VETO/INVALIDATE").

Sometimes that early exit **banks a partial win** the trade would otherwise
round-trip to its stop. Sometimes it **cuts a runner** that was about to reach
TP. Until 2026-09-16 there was no way to ask which dominates: an AUD/NZD H1
replay closed at +0.75R on `07-close-on-sr-reversal` (2026-07-30) and no flag
could turn it off — `--skip-bcr` and `--skip-calendar-bars` don't touch it.

## What shipped

- **`--skip-reversals`** — drops BOTH closes, suppressed at the source in
  `build_trade_spec` so neither alert is emitted. Includes the default-on TP
  resistance band, which is an S/R band like any other and would otherwise keep
  the close armed on a cell whose name says reversals are off.
- **`--reversal-matrix`** — an opt-in `--save-matrix` axis that gives every cell a
  `-rev-off` twin (8 → 16, multiplying through `--sl-matrix` / `--entry-matrix`).
  **Off by default.**

Exits only: the `too-high`/`too-low` invalidation caps and the 80%-to-TP
pcl-exhausted abort are VETOS, fire independently, and do not close a position.
H&S only — M/W hardcodes both fields off.

## The pilot (8 setups, 2026-09-16) — UNDERPOWERED, do not cite as a verdict

Ran the 16-cell matrix over the first 8 specs alphabetically, then collapsed the
64 cells to 8 independent setups (the 8 entry-rule columns of one setup are
**one trade counted eight times**, not eight observations).

| setup | effect of turning reversals OFF |
|---|---|
| aud-cad-h1-2026-07-22 | **+0.955** |
| aud-cad-h1-2026-07-23 | −0.499 |
| aud-cad-h1-2026-07-28 | 0.000 |
| aud-cad-h1-2026-08-05 | −0.365 |
| aud-chf-h1-2026-08-06 | 0.000 |
| aud-chf-m15-2026-08-13 | 0.000 |
| aud-jpy-h1-2026-09-01 | **−0.962** |
| aud-nzd-h1-2026-06-11 | 0.000 |

**Mean −0.109R per setup** — i.e. keeping the close ON was slightly better.

**Why that number means nothing yet:**

- It moved only **4 of 8** setups. Among those four: mean −0.218R, **stdev
  0.823** — nearly 4× the mean.
- Two cases dominate in **opposite directions and nearly cancel** (+0.955 and
  −0.962). Drop either one and the sign of the whole result flips.
- Not a random sample: first 8 alphabetically, **5 of them AUD/CAD**.

## The two cases that make the question real

Both mechanisms are confirmed to happen. This is the useful part of the pilot.

**The close cut a runner** — `aud-cad-h1-2026-07-22`:

| | reversals ON | reversals OFF |
|---|---|---|
| outcome | `reversal_closes: 1` | `tp_hits: 1` |
| R | **+0.26** | **+1.22** |

**The close banked a win** — `aud-jpy-h1-2026-09-01` (`skip-bcr` columns):

| | reversals ON | reversals OFF |
|---|---|---|
| R | **+3.77** | **+0.65** |

So it does both. Which dominates is an empirical question the full corpus could
answer and 8 setups cannot.

## Why the axis is opt-in rather than always-on

It shipped always-on (doubling the standard matrix 8 → 16) and was made opt-in
the same day, on the operator's call:

> *"have the `--skip-reversals` cli handy, in case i want to do individual
> replays to test it out, but, don't make it a matrix column yet. That way we
> won't unnecessarily blow out the matrix, but we can test it occasionally until
> we get more data"*

The cost is standing: an always-on axis doubles **every** future corpus
regeneration, forever, for a question asked occasionally. At an effect size
indistinguishable from noise on the available data, that is not a trade worth
making yet. `--skip-reversals` answers it per-replay at zero standing cost.

## To finish the measurement

Run the matrix with the axis on across all ~82 specs (~1300 cells), then collapse
to per-setup means as above:

```sh
export PATH="$PWD/target/release:$PATH"      # tv-arm shells out to replay-candles
for spec in replay-fixtures/*.spec.json; do
  name=$(basename "$spec" .spec.json)
  sym=$(python3 -c "import json;print(json.load(open('$spec'))['chart_symbol'])")
  inst=$(instrument-lookup to tradenation "${sym#TRADENATION:}" --json | python3 -c "import json,sys;print(json.load(sys.stdin)['symbol'])")
  ./target/release/tv-arm --spec-in "$spec" --save-matrix --reversal-matrix \
    replay --instrument "$inst" --fixtures-dir "$PWD/replay-fixtures" --save "$name"
done
```

Gotchas found the hard way during the pilot:

- **The spec's instrument field is `chart_symbol`** (e.g. `TRADENATION:AUDCAD`),
  not `instrument`. Passing an empty `--instrument` silently lets the chart
  symbol win — see the memory `replay_candles_reads_chart_symbol`.
- **`replay-candles` must be on `PATH`** — `tv-arm --replay` shells out to it and
  fails with a bare `No such file or directory` otherwise.
- **Not every instrument is in the baked spread table.** Amazon is refused
  outright (*"arming would size stops with no spread forecast"*) — correct
  behaviour, but it means index/equity setups need `TV_ARM_ALLOW_UNBAKED=1` or a
  re-bake before they can be included.
- **Collapse to setups before averaging.** Averaging the raw cells weights each
  setup by however many entry-rule columns it has and manufactures significance
  that isn't there.

If the full run still lands inside noise, the answer is "the close is roughly
R-neutral and can stay on for its risk-reduction value" — a legitimate outcome,
worth recording here rather than re-asking a third time.
