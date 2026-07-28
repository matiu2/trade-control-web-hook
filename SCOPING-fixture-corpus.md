# SCOPING — fixture corpus: re-armable trade specs + scored batch replay

**Origin:** `the-trading-academy/books/demo-journal/FEATURE-REQUEST-save-fixtures.md` (2026-07-26)
**Status:** scoping — awaiting sign-off, no code yet
**Date:** 2026-07-26

---

## 1. What the journalling side asked for

A **6-cell entry-sensitivity grid** on every one of 291 journal pages: three
entry rules (`normal` / `--skip-bcr` / `--strategy-v2`) × news calendar on/off,
each cell the run's accumulated Net R. Aggregated, that answers three standing
questions with data instead of anecdote:

- Does the **BCR gate** net-save or net-cost R?
- Does the **v2 confirming candle** net-save or net-cost R?
- Does the **news calendar** net-save or net-cost R?

Proven end-to-end on one trade (EUR/USD H1 H&S, TradeNation): the `normal` cell
reproduced the page's independently-derived **+0.52R** exactly.

Their five asks, priority as they ranked them:

| # | Ask | Their priority |
|---|-----|---|
| 4.1 | `net_r` (+ `legs[]`) in `expected.json` | **Critical** |
| 4.2 | `arm` block in `meta.json` | **High** |
| 4.4 | Batch `--test-mode` + `--json` | **High** |
| 4.3 | `--save-matrix` one-pass variants | Medium |
| 4.5 | `journal_ref` provenance | Low |

## 2. What we're actually building (and why it's more)

Two operator decisions expanded the scope beyond the request:

### 2a. Freeze the SPEC, not just the plan

The request's fixture freezes a resolved **`TradePlan`** + candles. That catches
"did engine behaviour change", but is **blind to `tv-arm` plan-building changes**
— a frozen plan replays its old geometry forever. If the way we pick
invalidation, compute TP from the fib, or lay out preps changes, a plan-level
fixture cannot see it.

The operator's framing: freeze the **inputs** and **re-arm a fresh plan** each
run. That covers the whole pipeline (geometry → plan → replay), and makes "which
entry rule earns more R" a *live* question rather than a frozen one. It also
kills the wrong-drawing risk permanently: the operator confirms the correct
pattern **once**, the levels are frozen, and no future run can pick the 2010
drawing.

So there are **two artifacts** with different jobs:

| Artifact | Freezes | Catches changes in | Question |
|---|---|---|---|
| **Spec fixture** (new) | inputs + levels | tv-arm plan-build **and** engine | "what would we do with this setup today?" |
| **Plan fixture** (exists, extend) | plan + candles | engine only | "did the engine's verdict on this exact plan move?" |

`--save-matrix` (4.3) then becomes what the operator described: a convenience
that pre-runs the variants so the grid appears immediately — not the thing that
gives the corpus its value.

### 2b. Two tiers, because a bug fix SHOULD move numbers

At 291 trades, one legitimate engine fix could break hundreds of `assert_eq!`
goldens. The instinct "then only ever add new features, never change existing
ones" is the wrong conclusion — most recent engine work (break-and-close
zone-straddle, retest slope-scaled tolerance, QM/v2 confirmation) *should* move
R on historical setups. If a bug fix moves nothing, it either didn't matter or
it isn't fixed.

The real problem is that a flat `assert_eq!` corpus yields **one bit** —
"300 failed" — when what's needed is *which way and by how much*. So:

- **Tier 1 — the gate (small, strict).** ~20 hand-picked fixtures, each pinning
  a deliberately-verified behaviour. `assert_eq!` on the full outcome, runs in
  `cargo test`. This is `replay-fixtures/` as it exists today; it stays this
  size.
- **Tier 2 — the corpus (large, scored).** All 291. **Not a pass/fail test** and
  **not in `cargo test`** — a batch tool emitting the aggregate and a diff
  against the last blessed baseline:

  ```
  vs baseline v113 → v114
    net R:  +42.1 → +47.8  (+5.7)
    moved:  38 trades  (29 improved, 9 worse)
    worst:  trade-071 −1.00 → −2.00
            trade-118 +1.18 →  0.00
  ```

  The operator then inspects the handful that got worse, decides whether the
  losses are legitimate, and re-blesses the batch in one go.

A blessed baseline is `(corpus, engine_version, aggregate)` — which is why
4.2's `engine_version` is load-bearing, not decorative.

## 3. Findings that shape the design

### 3.1 Everything 4.1 needs is already in memory — and there's a bug next to it

`fire.realized` (a `RealizedOutcome` from the `ReplayBroker` held ledger)
already carries per-leg direction, `fill_at`, `until`, `entry_price`,
`stop_loss`, `take_profit`, `exit_price`, and `kind`. The Net R tally exists
too — but `struct Tally` is **private to `report.rs`** (`report.rs:384-422`),
built during rendering and thrown away. Nothing serializes it.

**The bug:** the printed report and the saved fixture are computed by **two
different paths**.

- `report.rs` → `resolve_fire_any` → `fire.realized` (broker ledger)
- `fixture.rs` → `fill_for` → `fill_sim::simulate_fill` (independent re-sim)

The doc comment at `fixture.rs:16-19` still claims *"Both the report and the
snapshot compute their fill via the single `fill_for` path, so they can't
diverge"* — **stale; they diverged.**

This is not cosmetic. `simulate_fill` has **no reversal- or expiry-close
awareness**, so `expected.json` literally cannot represent those outcomes.
There is already a workaround test
(`stateful_broker_books_reversal_and_expiry_closes_in_the_report`,
`fixture.rs:387`) that asserts on **report text** instead of the golden, with a
comment explaining why. So today a regression turning a reversal-close into a
0R no-op **passes the fixture gate**.

The request's line — *"a regression that silently halves Net R while firing the
same rules currently passes the gate"* — is true, and worse than they knew.
Rewiring `expected.json` onto `fire.realized` fixes the divergence, unblocks
4.1, and makes `--check` cover reversal/expiry for the first time.

### 3.2 `TradeSpec` is already 90% of the re-armable artifact

Two parallel paths run out of the chart:

- **Path A (serializable):** drawings → `Roles` → **`cli::TradeSpec`** → signed
  alert bundle. `TradeSpec` (`cli/src/trade_patterns.rs:318`) derives
  Serialize/Deserialize, is already persisted as `trade.yaml`, and its doc
  comment says *"for reproducible rebuilds"*.
- **Path B (NOT serializable):** `Roles` — holding raw `Drawing` values — is
  passed **straight into `build_trade_plan`**, bypassing `TradeSpec` entirely.

`build-trade --from-file trade.yaml` already works with no TradingView
(`cli/src/bin/trade_control.rs:784`), but stops at the alert bundle — it never
builds a `TradePlan`, because `build_trade_plan` demands `&Roles`.

**The precedent that settles the design:** M/W already has **`MwSpec`**
(`trade_patterns.rs:639`) on `TradeSpec` — plain floats for
neckline/first-point/runup-start/right-shoulder, no drawings. H&S has no
equivalent. That gap is the entire job.

### 3.3 Exactly six `roles.*` reads must become spec fields

From `trigger_for` (`tv-arm/src/trade_plan_build.rs:264`):

| `roles` read | Line | Needed as |
|---|---|---|
| `roles.break_and_close` → two `LinePoint` | :297, `trendline_trigger` :441 | neckline anchors `(epoch, price)` ×2 |
| `roles.retest` (same neckline) | :304 | — (reuses neckline; drawn retest already ignored, `roles.rs:241`) |
| `roles.invalidation` → `horizontal_level` | :377, :487 | invalidation level (float) |
| `roles.tp_fib` → `fib_head_neckline()` | :392-394 | fib head + neckline (2 floats) |
| `roles.trade_expiry` → epoch | :278, `time_trigger` :431 | expiry epoch |
| `roles.prep_expiries` → epochs | :279-285 | per-step epochs (spec has names only) |
| `roles.mw_path` | :339-341, :405 | M/W-only; `MwSpec` exists but is bypassed |

Plus **granularity** (feeds `TrendlineCross.bar_seconds`, :463).

### 3.4 The operator's input list maps cleanly

H&S *points* aren't always available and aren't needed — the plan build only
ever consumed a few **levels and lines**; the points existed to derive them.

| Operator's input | Status |
|---|---|
| 1. too-high / too-low | → invalidation level (new field) |
| 2. support / resistance | ✅ `sr_reversal_ranges` already on `TradeSpec` |
| 3. strategy flags (skip-bcr, v2, …) | ✅ `strategy_v2`, `skip_preps`, `entry_mode`, `needs_golden` already there |
| 4. fib (TP level) | ⚠️ `tp_price` there; **fib endpoints** needed for the pcl-exhausted 80% level |
| 5. trade-expiry | ✅ already there as `DateTime` |
| + neckline anchors | **new** — `TrendlineCross` needs the line; its slope drives retest tolerance |
| + right-shoulder mid | **new** — usually `--start` but *not always*, so carry it explicitly |
| + granularity | **new** |

### 3.5 Freeze vs re-read — getting this wrong bakes in wrong numbers

| **Re-read every arm** | Why |
|---|---|
| Broker spread (M/W, `spread.rs:40`) | a frozen spread mis-sizes entry |
| Live mid (`--pull-back` anchor, `spread.rs:66`) | it *is* "price at arm time" by definition |
| Calendar / news windows (`pipeline.rs:1913`) | a function of the new arm time |
| instrument-lookup pip/tick | pure local catalog (`LazyLock`), free |

| **Freeze** | Why |
|---|---|
| Geometry (§3.3 + §3.4) | that's the setup |
| Strategy flags | that's the variant under test |
| Arm cursor / right-shoulder mid | pins reproducibility |
| Granularity | property of the setup |

**ATR is not in `tv-arm` at all** — the engine computes it from candles. Nothing
to freeze. TV symbol precision already lands on `TradeSpec.pip_size`/`.tick_size`
so it's freezable today; re-reading is also safe.

### 3.6 Free win

`env!("GIT_VERSION")` (`git describe --tags --dirty --always`) is already baked
into the `cli` crate by `cli/build.rs:8`. `engine_version` needs **no build
changes**. Note `replay-candles` has no `#[command(version)]` wired up — worth
adding while we're there.

## 4. Schema

### 4.1 `expected.json` — add `outcome`

```json
"outcome": {
  "net_r": 0.52,
  "fires": 9,
  "tp_hits": 0, "sl_hits": 1, "reversal_closes": 1, "expiry_closes": 1,
  "legs": [
    {"entry_time":"2026-07-20T12:00:00Z","entry_price":1.14281,
     "stop_loss":1.14405,"take_profit":1.13900,
     "exit_time":"2026-07-20T12:00:00Z","exit_price":1.14238,
     "exit_reason":"reversal","r":0.35}
  ]
}
```

`exit_reason` is the serialized `FillKind` (`stopped_out` / `took_profit` /
`reversal` / `expiry` / `open` / `never_filled` / `gate_blocked`). Sourced from
`fire.realized`, **not** a re-sim. Additive + `#[serde(default)]` so existing
fixtures still load.

### 4.2 `meta.json` — add `arm`

```json
"arm": {
  "entry_rule": "normal",
  "skip_calendar_bars": false,
  "skip_bcr": false,
  "strategy_v2": false,
  "skip_golden": false,
  "start": "2026-07-17T17:00:00+10:00",
  "broker": "tradenation",
  "chart_symbol": "TRADENATION:EURUSD",
  "tv_arm_version": "v113-4-gabc123",
  "engine_version": "v113-4-gabc123"
}
```

`chart_symbol` is recorded **broker-qualified** so a wrong-feed capture (bare
`EURUSD` silently resolving to OANDA — appendix gotcha) is findable later rather
than silently diluting the aggregate.

### 4.3 `HsSpec` on `TradeSpec` (new, mirrors `MwSpec`)

```rust
/// Frozen H&S geometry — the plain-data form of what `Roles` carries as raw
/// `Drawing`s, so a plan can be rebuilt with no TradingView. Mirrors `MwSpec`.
/// All prices are MID.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HsSpec {
    /// Neckline anchor A — `(epoch, price)`. Drives `TrendlineCross`; the
    /// A→B slope drives the retest tolerance.
    pub neckline_a: LinePoint,
    pub neckline_b: LinePoint,
    /// The drawn invalidation level (too-high for a short / too-low for a long).
    pub invalidation: f64,
    /// Fib endpoints — head and neckline. TP is `2×neckline − head`; the
    /// pcl-exhausted abort is the ~80%-to-TP level derived from the same pair.
    pub fib_head: f64,
    pub fib_neckline: f64,
    /// Middle of the right shoulder. Usually the arm cursor but NOT always,
    /// so it is carried explicitly rather than inferred from `--start`.
    pub right_shoulder_mid: f64,
    /// Bar size the pattern was read at — feeds `TrendlineCross.bar_seconds`.
    pub granularity: Granularity,
    /// Prep-expiry epochs by step name (the spec carries names only today).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prep_expiries: Vec<(String, i64)>,
}
```

`trade_expiry`, `sr_reversal_ranges`, `tp_price`, and every strategy flag stay
where they already are on `TradeSpec`.

## 5. Commit breakdown

Each commit green (tests + clippy + fmt) before the next. Sizes are estimates.

| # | Commit | Ask | ~Lines | Notes |
|---|---|---|---|---|
| 1 | Extract `Tally` → public `ReplayEconomics`; `report.rs` and `fixture.rs` both consume it | pre-4.1 | ~200 | The shared-outcome refactor. **Fixes the report↔fixture divergence** (§3.1). Retire the report-text workaround test. |
| 2 | `outcome{net_r, counts, legs[]}` in `expected.json`, off `fire.realized` | 4.1 | ~180 | Additive schema. Re-bless the ~5 existing fixtures. |
| 3 | `arm{}` block in `meta.json` + `--trade-ref`/`journal_ref`; wire `GIT_VERSION` into `replay-candles --version` | 4.2, 4.5 | ~150 | 4.5 rides along — same struct, trivial once 4.2 lands. |
| 4 | Batch `--test-mode --fixtures-glob` + `--json` | 4.4 | ~250 | Machine-readable; one JSON object per fixture. |
| 5 | `HsSpec` on `TradeSpec`; `build_trade_plan` reads spec not `&Roles` | 2a | ~400 | **The biggest and riskiest.** Pure refactor — byte-identical plans for existing arms, guarded by a round-trip test. |
| 6 | `tv-arm --spec-in` — arm from a frozen spec, no TV | 2a | ~250 | Re-read spread/mid/calendar per §3.5. |
| 7 | Tier-2 scored corpus tool: aggregate Net R, baseline diff, re-bless | 2b | ~350 | Not in `cargo test`. |
| 8 | `--save-matrix` one-pass variants | 4.3 | ~200 | Convenience; everything above works without it. |

Appendix gotchas, folded in where cheapest:

- **`--start` strict RFC3339** (seconds mandatory; `17:00+10:00` rejected) → fix
  with commit 6, aligning `tv-arm --start` with the `replay` passthrough's bare
  Brisbane.
- **`--annotate` collision** → **reproduce first.** `replay.rs:68-81` injects
  defaults *before* passthrough and clap is last-wins, with a test at
  `replay.rs:177-196` asserting the override works. The report says it fails.
  Verify before "fixing". A `--no-annotate` for unattended batch runs belongs
  with commit 4 or 8 regardless.
- **Broker-qualified chart symbols undiscoverable** → recorded in `arm{}`
  (commit 3); the `instrument-lookup` side is a separate change in that repo.
- **Wrong-drawing risk** → structurally solved by commits 5+6 (confirm once,
  freeze, re-arm forever). An `--expect-rs <price>@<time>` guard is optional
  once the spec path exists.

## 6. Out of scope, noted

**Scaled exits** (operator: 50% off at 80%-to-TP, up to 90% at TP, let the rest
run) and other future strategies. This is a bigger change than a flag: today's
replay books **one R per leg**, and partial exits mean a position closing in
tranches — touching the broker ledger, `realized_r`, and `Tally`/`ReplayEconomics`.

Worth stating **why the spec-level artifact matters for it**: a frozen *plan*
could never evaluate a new exit strategy (its old single-exit rules are baked
in). A frozen *spec* re-arms through today's `tv-arm`, so a new strategy flag
becomes a new grid column across all 291 trades on the day it ships. Commit 5's
`ReplayEconomics.legs[]` should therefore be shaped to tolerate multiple exits
per entry later, even though nothing emits that today.

**Adjacency risk:** `SCOPING-engine-v2-typed-geometry.md` describes a
`trade-control-types-v2` crate holding `TradePlan`/`Line`/`PriceLevel`/
`TimeMarker` as shared serializable data with a future `tv-arm-v2` builder —
which would create the §3.2 seam properly. Operator decision (2026-07-26): add
`HsSpec` to v1 `TradeSpec` now, accepting that commit 5 may be partly redone
when v2 lands. Rationale: it's small, and it unblocks the 291-trade capture pass
immediately.

## 7. What this unlocks

1. **One live pass per trade** — operator present, confirms the correct pattern,
   saves the spec. Touches TradingView exactly once, ever.
2. **Offline forever after** — regenerate all 291 grids in seconds with no TV and
   no broker, and *re-generate* after every engine change to see which
   conclusions moved.

Step 2 is the prize. It turns the entry-rule/news question from a one-shot manual
survey into a **standing regression surface**: every future engine change gets
scored against 291 real historical setups before it ships — not "did tests pass"
but "did this earn or cost R across the whole book." Given the promotion clock
(a clean staging week → $1k → scale), that is a materially better safety net than
the ~20-fixture gate we have today.
