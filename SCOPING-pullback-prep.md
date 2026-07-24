# SCOPING — `--pull-back` prep alternative + either/or prep encoding

Status: **discussion / plan** (not started). Author: Claude, 2026-07-24.

## What the operator asked for

- A new prep: instead of a **retest** (price returns to the neckline), accept a
  **pullback** — price retraces **≥ 1 ATR** (adjustable) after the break.
- So the setup becomes: *break-and-close → pullback* as an alternative to
  *break-and-close → retest*.
- Keep retest available. The `requires_preps` field becomes a **vec of vecs**:
  `[break-and-close, [retest, pullback]]`. An **inner vec = either/or (OR)**.
- Flag: `--pull-back`, optionally with a value `--pull-back=1.5` = ATR multiple.
- Open question the operator raised: could this vec-of-vecs encoding also
  simplify the "confirmed candle vs retest candle" split?

## Current architecture (verified in tree)

Two parallel engines exist. **Only v1 is live** (worker + replay). v2
(`engine-v2`, `trade-control-types-v2`) is a parallel rewrite, not in the live
path.

### The flat prep list (v1, the live path)

`core/src/intent.rs:506`
```rust
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub requires_preps: Vec<String>,   // e.g. ["break-and-close", "retest"]
```
- **Ordered AND**: every named prep must be set, each prep's `set_at` strictly
  after the previous. Enforced in `core/src/dispatch/enter.rs:192-243`
  (`for step in &requires_preps { ... prev_ts ... }`).
- Assembled in `cli/src/trade_patterns.rs:2172` from the literal
  `["break-and-close", "retest"]` minus `skip_preps`.

### Retest today

- Alert `04-prep-retest` built in `cli/src/trade_patterns.rs:1951`
  (`build_retest_alert`, `intent.step = "retest"`).
- Trigger: `Trigger::TrendlineCross { … bar: Intrabar }` (opposite cross of the
  neckline) — `tv-arm/src/trade_plan_build.rs:284`.
- Engine stamp: `stamp_retest` / `retest_satisfied` / `retest_tolerance` /
  `neckline_slope` in `engine/src/evaluate.rs:1544-1786`; state field
  `PlanState.retest_seen_at`. Retest is **not a Phase** — it's a lookback
  stamp inside `Phase::AwaitEntry`, gated to `(break_close_at, entry]`.
- `break_close_at` is stamped when the neckline is broken-and-closed
  (`engine/src/evaluate.rs:714`). This is the natural reference for a pullback.

### Trigger enum

`core/src/trade_plan.rs:305` — `HorizontalCross`, `PriceValueCross`,
`TrendlineCross`, `TimeReached`, `MwEveryBar`, `PinePattern`. A pullback is
**new geometry** (ATR distance from the break), so it needs a new variant, e.g.
`PullbackFromBreak { atr_mult, dir }`, resolved against `break_close_at` +
the ATR window.

## What a "pullback" is (DEFINITION — operator-confirmed 2026-07-24)

**Body-extreme retrace ≥ N×ATR since ARM TIME, in the entry direction.**

Vocabulary (three distinct moments — do not conflate):
- **arm time** = when the operator runs `tv-arm`; the plan is created, signed,
  and uploaded to the server. Recorded as `TradePlan.armed_at`. **← the pullback
  anchor.**
- **placement time** = when a prep-gated `enter` order is placed.
- **fill time** = when that order fills at the broker.

Definition (for a **long**):
- **Anchor** = the **mid open of the candle live at arm time**, baked onto the
  signed plan by `tv-arm` (NOT rediscovered by the engine).
- **Running high** = the highest `max(open, close)` (body, not wick) of each bar
  **from `armed_at` forward**.
- **Pullback fires** when the current bar's **open or close** is **≥ N×ATR below**
  that running high.

**Short** = mirror: running low = lowest `min(open, close)` since arm; fires when
the current open/close is **≥ N×ATR above** it.

Key properties:
- **Bodies, not wicks** — a liquidity-vacuum wick can neither set a false extreme
  nor falsely trigger. Dovetails with spread-hour rubbish-candle suppression.
- **Independent of any break.** Pullback anchors to arm time whether or not a
  break-and-close prep exists (operator can arm `--skip-break-and-close
  --pull-back`). The `(break_close_at, entry]` prep-order gating only decides how
  prep *milestones* order into the enter gate — it does NOT move the pullback's
  price anchor.
- N default = **1.0**, override `--pull-back=1.5`. ATR length =
  `atr_length_for(granularity)` (same as retest). **Fail-closed** (skip + `warn!`)
  on an ATR-starved window — never `.expect()`-panic (cf.
  `[[retest_tolerance_panic_kills_cron_loop]]`).
- `FireMode::Once` — a milestone the trade passes once; latch on
  `pullback_seen_at`.

### Anchor is BAKED, extreme is SCANNED

- **`anchor_open` (mid) is baked** onto the trigger by tv-arm at arm time — the
  engine never has to locate "the arm-time bar" in its window (ambiguous if the
  window is short or the arm was mid-bar). Deterministic, signed, replay==live.
- **The running body-extreme is a stateless re-scan** of the engine's `window`
  for bars in `[armed_at, now]` each call (same style as `retest_tolerance`,
  which re-derives from the window every call). **No new `PlanState` field** — as
  long as the window reaches back to `armed_at`, which it does (arm time ≤ the
  plan's earliest drawn anchors). `pullback_seen_at` is the only new state (the
  latch), mirroring `retest_seen_at`.

### `armed_at` is promoted from journalling-only to load-bearing

Today `TradePlan.armed_at` is journalling-only (`core/src/trade_plan.rs:242`,
`[[armed_at_and_sentiment_journalling]]` — "never gates or schedules off it").
The pullback makes it load-bearing (it bounds the extreme scan). Since the
anchor *price* is baked separately, a missing/absent `armed_at` on a legacy plan
just means "no pullback prep was armed" — back-compat holds (no pullback trigger
⇒ the field is never read for gating).

**Alternatives considered & rejected:**
- *Anchor = break-candle close*: can't — pullback may be armed with no break.
- *Anchor = post-break extreme*: superseded; arm-time anchor is more general.
- *Wick extreme*: rejected — bodies avoid spread-hour false extremes.
- *Derive anchor from `armed_at` + window scan instead of baking*: rejected —
  reintroduces window-reach / ATR-starve fragility for no benefit.

## The either/or encoding

### Type change (v1)

`core/src/intent.rs`:
```rust
#[serde(untagged)]
pub enum PrepReq {
    All(String),        // "break-and-close"
    Any(Vec<String>),   // ["retest", "pullback"]  -> OR
}
pub requires_preps: Vec<PrepReq>,
```
Wire form stays readable: `[break-and-close, [retest, pullback]]`. A bare
string ⇒ `All`; a nested list ⇒ `Any`. `#[serde(untagged)]` makes a plain
`[break-and-close, retest]` still parse (each becomes `All`), so **every
existing signed intent / YAML template round-trips unchanged**. This is the
key back-compat property — the HMAC body is unchanged for legacy plans.

### Gate change (v1)

`core/src/dispatch/enter.rs:192-243` — replace the flat loop:
- `All(step)`: exactly today's behaviour (present, `set_at > prev`).
- `Any(alts)`: pass if **at least one** alt is set with `set_at` strictly after
  `prev`; that satisfying alt's `set_at` becomes the new `prev`. (RECOMMENDED —
  keeps the strictly-increasing ordering across groups. Confirm vs the looser
  "any set, ignore ordering" option.)

### Engine change (v1 replay parity — MANDATORY, same-decision-in-both rule)

Per `[[strategy_changes_in_both_replayer_and_worker]]`, the replay engine must
make the identical decision. `engine/src/evaluate.rs`:
- New `stamp_pullback` mirroring `stamp_retest`, stamping
  `PlanState.pullback_seen_at`.
- The enter gate (`evaluate_entry`, ~`:919`) currently computes
  `requires_retest`; generalise to walk the new `Vec<PrepReq>`:
  a group is satisfied if any member's `*_seen_at` is set in
  `(break_close_at, entry]`. `retest` ⇒ `retest_seen_at`, `pullback` ⇒
  `pullback_seen_at`.

## Emitting a `pullback` prep (build-trade pipeline)

- New `AlertBasename::PrepPullback` → `04b-prep-pullback` (or reuse the `04-`
  slot family) in `conventions/src/basenames.rs` + `parse`.
- New `RuleKind` variant in `conventions/src/roles.rs` + `From<AlertBasename>`.
- `build_pullback_alert` beside `build_retest_alert`
  (`cli/src/trade_patterns.rs:1951`), `intent.step = "pullback"`.
- Trigger build arm in `tv-arm/src/trade_plan_build.rs` producing the new
  `Trigger::PullbackFromBreak { atr_mult, dir }`. No drawing needed — the
  pullback is computed, not drawn (unlike the retest trendline). This is
  simpler than retest: no `roles.retest` role, no neckline anchors.
- `KNOWN_PREP_NAMES` (`cli/src/trade_patterns.rs:885`) gains `"pullback"`.

## tv-arm flag

`tv-arm/src/args.rs`:
```rust
/// Accept a pullback (retrace ≥ N×ATR after the break) as an ALTERNATIVE to
/// the retest. Bare `--pull-back` uses 1.0 ATR; `--pull-back=1.5` overrides.
#[arg(long, num_args = 0..=1, default_missing_value = "1.0")]
pub pull_back: Option<f64>,
```
- When set: the enter's prep group for the retest slot becomes
  `Any([retest, pullback])` (or `Any([pullback])` if combined with
  `--skip-retest`). Wire the atr_mult onto the pullback trigger.
- `apply_aliases` (`tv-arm/src/args.rs:667`) untouched unless we want
  `--pull-back` to imply `--skip-retest` (it should NOT by default — operator
  wants BOTH available).
- `skip_preps` plumbing (`tv-arm/src/pipeline.rs:1191`) learns `pullback`.

## The "confirmed vs retest" unification question

`confirmed` (Pine signal candle on the enter, `needs_confirmed`/`needs_golden`)
and `retest` (a prep milestone) are **different axes today** — one is a
property of the enter bar, the other a store-backed prep gate. The vec-of-vecs
`Any` group *could* express "confirmed OR retest OR pullback" **iff** we model
`confirmed` as a prep too (stamp a `confirmed` prep when the signal candle
prints). That's a larger refactor (moves a per-bar enter property into the
prep store). **Recommendation: ship pullback first with the `Any` encoding,
then evaluate folding `confirmed` in as a follow-up** — the encoding is the
enabler, but conflating the two axes in one PR risks the confirmed-first
selection filters (`[[signal_criteria_refactor]]`).

## Rollout / PR slices (each <600 lines, tests-first)

1. **PR-A — encoding only (no new prep). ✅ DONE (branch
   `pullback-prep-encoding`, not merged).** `PrepReq { All(String) |
   Any(Vec<String>) }` (`#[serde(untagged)]`) in new module
   `core/src/intent/prep_req.rs`, with the pure ordered-OR decision extracted to
   `resolve_slot` (unit-tested in isolation — All/Any × satisfied/out-of-order/
   missing/within-group-ordering). `requires_preps: Vec<String>` → `Vec<PrepReq>`.
   Gate loop in `core/src/dispatch/enter.rs` rewritten as a thin store-fetch →
   `resolve_slot` shim. Engine readers (`evaluate.rs`) use the new
   `PrepReqSliceExt::requires_step`. cli assembly wraps each surviving prep in
   `PrepReq::All` (still emits the flat `[break-and-close, retest]` — either/or
   grouping is PR-C). Legacy flat wire form round-trips byte-identically
   (`legacy_flat_list_round_trips_byte_identically`) ⇒ no HMAC change to any
   in-flight plan. Full workspace test suite green; clippy/fmt clean on touched
   files. **No behaviour change to any live plan.**
2. **PR-B — pullback trigger + engine stamp.** New `Trigger::PullbackFromBreak`,
   `stamp_pullback`, `pullback_seen_at`, `retest_satisfied`-style
   `pullback_satisfied`. Engine tests (fires after N×ATR retrace; respects
   `(break_close_at, entry]`; fail-closed on ATR-starved window).
3. **PR-C — build-trade + tv-arm flag. ✅ DONE.** `AlertBasename::PrepPullback`
   (`04b-prep-pullback`) + round-trip, `RuleKind::PrepPullback`,
   `build_pullback_alert` (cli), the either/or group assembly in
   `build_enter_alert` (via `PrepReq::from_alternatives`), the
   `trigger_for` → `Trigger::PullbackFromArm` arm (anchor baked via a new
   `PullbackArm` threaded through `build_trade_plan`), the `--pull-back` clap flag
   (bare = 1.0, `=1.5` overrides), `TradeSpec.pull_back`, and the arm-time live-mid
   anchor read (`spread::read_mid` + `read_mid_blocking`). End-to-end verified:
   `tv-arm … --pull-back` emits `04b-prep-pullback` and an enter with
   `requires_preps = [break-and-close, [retest, pullback]]`;
   `--pull-back --skip-retest` collapses to `[break-and-close, pullback]`; no flag
   ⇒ byte-identical legacy `[break-and-close, retest]`. Full suite green;
   clippy/fmt clean on touched files. **Note:** `KNOWN_PREP_NAMES` intentionally
   NOT extended — it validates operator-typed `skip_preps`, and `pullback` isn't a
   skip target in the current flag design (a follow-up could add `--skip-pullback`).
4. **PR-D (optional/later) — confirmed-as-prep unification.**

Replay parity checked with an existing fixture through each PR (the
`replay-fixtures/` set). Deploy to `staging` per-PR since it's fast-moving.

## Keep the pullback logic in ONE module (operator request 2026-07-24)

Factor the pullback so its logic is self-contained and clear, not smeared across
`evaluate.rs` (matches the "small modules, no mod.rs, one concept per file"
convention). Target shape:

- **`engine/src/pullback.rs`** (NEW) — the pure, fully unit-testable core:
  ```rust
  /// Running body-extreme (max(open,close) for Long, min for Short) over bars
  /// in [armed_at, now]. None if no bars in range.
  pub fn body_extreme(window: &[Candle], armed_at: DateTime<Utc>, dir: Direction) -> Option<f64>;

  /// Has price retraced ≥ atr_mult×ATR from the running body-extreme, measured
  /// on THIS candle's open/close? Anchor is the baked arm-time mid-open.
  pub fn triggered(
      anchor_open: f64, window: &[Candle], armed_at: DateTime<Utc>,
      atr: f64, atr_mult: f64, dir: Direction, candle: &Candle,
  ) -> bool;
  ```
  No `PlanState`, no `TradePlan`, no I/O — slices + scalars in, bool out. All the
  edge cases (empty range, wick-vs-body, long/short mirror, N scaling) are tested
  here in isolation. Thoroughly doc-commented so the later v2 port is a copy.
- **`engine/src/evaluate.rs`** — only a thin `stamp_pullback` that: resolves ATR
  (fail-closed), calls `pullback::triggered`, and owns the `pullback_seen_at`
  latch + `push_fire`. Mirrors `stamp_retest`'s ~15-line shape. No pullback math
  lives here.
- **`core/src/trade_plan.rs`** — the `Trigger::PullbackFromArm { anchor_open,
  atr_mult, dir }` data variant (the enum stays one file; only the *evaluation*
  is modularised).
- **`tv-arm`** — a small `pullback` helper (in `trade_plan_build.rs` or its own
  `tv-arm/src/pullback.rs`) that reads the live arm-time candle mid-open and
  builds the trigger.

This keeps PR-B's new logic reviewable as a single file + a thin call-site.

## Files touched (index)

- `core/src/intent.rs` (PrepReq, requires_preps type, tests) — PR-A
- `core/src/dispatch/enter.rs` (gate loop) — PR-A
- `core/src/trade_plan.rs` (Trigger::PullbackFromBreak) — PR-B
- `core/src/plan_state.rs` (pullback_seen_at) — PR-B
- `engine/src/evaluate.rs` (stamp_pullback, gate generalisation) — PR-A/B
- `conventions/src/basenames.rs`, `conventions/src/roles.rs` — PR-C
- `cli/src/trade_patterns.rs` (build_pullback_alert, KNOWN_PREP_NAMES, :2172) — PR-C
- `tv-arm/src/args.rs` (--pull-back), `tv-arm/src/pipeline.rs` (skip_preps),
  `tv-arm/src/trade_plan_build.rs` (trigger arm) — PR-C
- YAML templates (`gbp-aud.yaml` etc.) — unchanged (untagged serde)

## Decisions

1. Pullback reference point: **OPEN — A (break-close) vs B (post-break extreme)**.
   C (neckline) rejected (= a fattened retest). A needs no new state; B needs one
   running-extreme `f64` on `PlanState`. Awaiting operator pick.
2. Engine target: **v1 only** (CONFIRMED 2026-07-24). But document the engine
   stamp + gate + trigger thoroughly so a later v2 port is mechanical — mirror
   the retest's v2 shape (`engine-v2/src/rules/retest.rs`, `PrepMap`).
3. Ordering: **CONFIRMED — pullback is just another prep**; the operator arms the
   order they want. `--skip-break-and-close --pull-back` ⇒
   `[[retest, pullback(1.0)]]`; `--skip-retest --pull-back=1.5` ⇒
   `[break-and-close, pullback(1.5)]`. The either/or `Any` group is formed at arm
   time from whichever of {retest, pullback} survive the skip flags. Within an
   `Any` group the gate keeps the strictly-increasing-`set_at`-vs-previous-group
   rule (any member may satisfy it).
4. `--pull-back` does **NOT** imply `--skip-retest` (CONFIRMED). Both preps are
   emitted and OR'd unless a skip flag drops one.

### Arm-time group formation (from #3)

`skip_preps` + `--pull-back` decide the `requires_preps` shape:

| flags | requires_preps |
|---|---|
| (none) | `[break-and-close, retest]` (today) |
| `--pull-back` | `[break-and-close, [retest, pullback]]` |
| `--skip-retest --pull-back` | `[break-and-close, pullback]` (single-member Any collapses to All) |
| `--skip-break-and-close --pull-back` | `[[retest, pullback]]` |
| `--skip-retest --pull-back=1.5` | `[break-and-close, pullback(1.5)]` |

A single-member `Any([x])` is equivalent to `All(x)` and should serialize as the
bare string for cleanliness. The atr-mult travels on the **pullback trigger**
(`Trigger::PullbackFromBreak { atr_mult }`), not in the prep-name string — the
name stays `"pullback"` so the store key + gate are simple; the multiple is a
property of the arming, baked onto the signed plan.
