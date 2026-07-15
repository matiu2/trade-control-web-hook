# SCOPING — engine-v2 typed fact-kinds + split geometry

Status: **design sketch for review, then build 4a→4d.**

## Why

Two decisions from the 2026-07 review:

1. **Fact kinds + line/geometry names become compile-time Rust types**, not
   strings. Everything fixed in the type system for strong robustness; we accept
   reduced flexibility for *unknown* future trading styles (there's nothing
   concrete to keep flexible for yet — revisit when trend-following lands with a
   real dynamic-geometry example). Kinds and geometry references stop being
   collision-prone strings.

2. **Split the single `Line` into three geometry kinds** that match what
   `cross.rs` *already* treats differently:
   - **`Line`** — a real (sloped or horizontal) line with two anchors, crossed by
     bar-index projection (`line_price_at`). Today: `Neckline`. Future:
     trendlines.
   - **`PriceLevel`** — a single horizontal price. `TooHigh`, `TooLow`. A "cross"
     is just `price ≷ level` — **no second anchor, no bar-index projection**.
   - **`TimeMarker`** — a single timestamp. `NewsEvent`, `Expiry`. Not a price
     cross at all — "has time T passed?".

   Key insight: the v1 `Trigger` enum *already* separates
   `HorizontalCross`/`PriceValueCross` (a level, no projection) from
   `TrendlineCross` (two anchors + projection) from `TimeReached` (a timestamp).
   engine-v2's plan model collapsed all three into `Line{a,b}` and reconstructs
   the distinction at eval time (`trigger_for` always builds a `TrendlineCross`,
   even for a horizontal neckline — a degenerate `a.price == b.price` trendline
   that still runs full projection it doesn't need). The split makes the plan
   model match the eval reality: a `PriceLevel` cross skips projection — a
   **correctness + clarity win**, not just a rename.

## Shared model lives in `trade-control-types-v2` (extracted 2026-07)

The v2 **plan model** + the `FactKind`/`LineName` marker traits were extracted
from `engine-v2` into a dedicated **`trade-control-types-v2`** crate. Rationale:
the model is *data* every v2 participant agrees on — a builder writes it, the
engine consumes it — whereas the engine's *behaviour* (`Rule`, `Effect`, `Facts`,
`driver`, `cross`, `late_entry`, the rule impls) is private machinery a builder
never needs. Splitting the two stops `tv-arm-v2` / `trade-control-v2` from having
to pull the whole engine in just to emit a `TradePlan`.

```text
        trade-control-types-v2   (TradePlan/Line/PlanRule/RuleKind/EntryMechanism
        ╱          │          ╲   + LineName & FactKind traits + markers)
  engine-v2    tv-arm-v2   trade-control-v2(cli)
```

- **In it:** `TradePlan`, `Line`, `PlanRule`, `RuleKind`, `EntryMechanism`,
  `PrepMap`; `LineName` + `Neckline`/`TooHigh`/`TooLow`; `FactKind` +
  `BreakClose`/`Retest`/`EntryOutcome`/`LastClose`. Deps: `serde` +
  `trade-control-core`.
- **NOT in it:** all behaviour (stays in `engine-v2`). `engine-v2` depends on
  types-v2 and **re-exports** it, so an engine consumer keeps one import surface.
- **Naming:** picked `trade-control-types-v2` (descriptive of *what it holds*).
  `tv-arm-v2` / `trade-control-v2` don't exist yet — they're the future builders
  that will depend on this crate directly.

## Vocabulary today (H&S + M/W only)

- **Lines:** `Neckline` (the only real line).
- **PriceLevels:** `TooHigh`, `TooLow` (invalidation / pcl-exhausted caps).
- **TimeMarkers:** `NewsEvent`, `Expiry`.
- **Fact kinds:** `BreakClose`, `Retest`, `EntryOutcome` (shared);
  `LastClose` (rule-private scratch).

## The type design (trait + zero-size markers, per-crate open set)

```rust
// A fact kind's stable serialized name. Zero-size marker structs implement it;
// a setup crate (H&S, M/W, future trend-following) can define its own kinds
// without a central enum.
pub trait FactKind { const NAME: &'static str; }
pub struct BreakClose;   impl FactKind for BreakClose  { const NAME: &str = "break_close"; }
pub struct Retest;       impl FactKind for Retest      { const NAME: &str = "retest"; }
pub struct EntryOutcome; impl FactKind for EntryOutcome { const NAME: &str = "entry_outcome"; }
pub struct LastClose;    impl FactKind for LastClose   { const NAME: &str = "last_close"; }
```

**Wire boundary (settled):** the plan is *just serialized* — tv-arm and the
`trade-control` CLI share the **same types crate** as the server, so `PlanRule`'s
geometry reference + `preps` serialize/deserialize through those shared types.
The marker types live in the shared crate; on the wire they are still their
`NAME` strings. No central runtime registry — deserialization maps a known name
back to its type at the point a rule consumes it, because **a rule knows its own
kinds/geometry at compile time** (the retest rule always references `Neckline` +
writes `Retest`; it never discovers a line name at runtime the way the
string-keyed version did).

### The line-name reality that shaped this

In the string version a rule read `self.rule.line` (a runtime `String` from the
plan) — so lines *looked* runtime. But every rule actually targets a
**compile-time-known** geometry for its setup (H&S retest → `Neckline`). The
"runtime line name" was only runtime because the plan model made it a string.
Fixing geometry in types removes that false dynamism. The cost — a brand-new
setup can't be expressed as pure plan data, it needs Rust types — is the
robustness/flexibility trade we accepted.

## Fact keying under the split

Today: `(line: String, kind: String)`. After: the shared-fact key becomes
`(geometry: GeoRef, kind: &'static str)` where `GeoRef` is a typed reference to a
`Line` / `PriceLevel` / `TimeMarker`. Rule-private scratch stays
`(rule_id, kind)` (unchanged; scratch is per-rule bookkeeping, not geometry).

`FactValue` (`At | Flag | Num`) is unchanged by this work. The
`EntryOutcome: Placed|Missed` rich payload remains deferred to the driver-wiring
slice (that's where it's written).

## Cross paths after the split

- **`Line`** → today's path: build the projection, `line_price_at`, then
  `level_crossed`. Unchanged logic (the proven `cross.rs`).
- **`PriceLevel`** → **new, simpler path**: no projection. Resolve the level
  directly, `level_crossed(level, dir, bar, …)`. This is what a horizontal
  neckline should have used all along; `too-high`/`too-low` stop being degenerate
  trendlines.
- **`TimeMarker`** → `candle.time >= marker` (the v1 `TimeReached` arm). No price
  math.

## Build order (each commit small, green, reviewed one at a time)

- **4a — FactKind marker types.** Convert `Facts::{set,get,at,is_set,num,
  scratch}` to take a `FactKind` (kinds only; geometry still string here).
  Define the 4 kinds. Migrate the 3 rules + tests. Self-contained collision win;
  independently valuable. **Lands first.**
- **4b — typed geometry NAMES, still one `Line` type.** Introduce typed geometry
  references (`Neckline`, `TooHigh`, `TooLow`, `NewsEvent`, `Expiry`) as the
  geometry key, but all still backed by `Line{a,b}` as today. Facts key on the
  typed geometry ref. No cross-path change yet. Decided at build time (2026-07):

  - **`LineName` trait + zero-size markers** in `plan/line_name.rs`, mirroring
    `FactKind` — an OPEN set (trait, not enum) so a setup crate names its own
    lines. `Neckline`/`TooHigh`/`TooLow` today. Serializes as `NAME` (wire
    unchanged).
  - **`Facts` becomes generic over BOTH axes**: `set::<K: FactKind, L:
    LineName>()`, `get`/`at`/`is_set`/`num` likewise, + the `_named(line, kind)`
    runtime siblings kept (the driver applying `WriteFact`, and the enter reading
    its `preps`-map line *names*, are genuinely runtime — same rule as 4a's kind
    split). Kills the last string-collision surface for a rule with a fixed line.
  - **A rule's fixed line is a type parameter**: `BreakAndClose<'r, L: LineName>`,
    `Retest<'r, L>` (both `PhantomData<L>`). The driver constructs them with the
    line their `RuleKind` implies — `RuleKind::BreakAndClose`/`Retest` → `Neckline`
    today. The geometry lookup uses `plan.line_typed::<L>()` (delegates to
    `line(L::NAME)`); the fact key uses `is_set::<BreakClose, Neckline>()`. No
    runtime line string inside a producer rule.
  - **`PlanRule.line: String` is REMOVED.** A producer rule's line is fixed by its
    rule *type*, not carried in the plan — so it stops being wire data. **This is
    a wire-format change** (older plans with a `line:` field: `#[serde(default)]`
    tolerated on read, dropped on write; no live v2 plans exist yet, so no
    migration). `Line.name: String` **stays** (the enter's `preps` map still keys
    lines by name to find `Line{a,b}` geometry — that lookup is runtime).
  - The **enter stays runtime line-keyed** — its `preps` map is `{ line_name ->
    [kinds] }` plan data; it iterates lines by name via `at_named`. Typed where the
    rule knows its line at compile time; `_named` where it's genuinely plan-driven.
  - **Serialize round-trip test** guards the string-on-the-wire boundary: a
    `TradePlan` → serialize → deserialize → same typed facts read back.
- **4c — split `PriceLevel` out of `Line`. DONE** (`f7a1441`→`ea7fb68`,
  feat/engine-v2-slice1). `TooHigh`/`TooLow` became `PriceLevel { name, price }`
  (single price) with a no-projection cross path — and, per the build-time
  decision to not ship a dead type, a working `Invalidate` rule consumes it.
  Landed in 5 reviewed/green steps:
  1. **`PriceLevel` model** in types-v2: `TradePlan.levels: Vec<PriceLevel>`
     (`#[serde(default)]`), `level`/`level_typed::<L>()` mirroring the line pair.
     Reuses the existing `TooHigh`/`TooLow` `LineName` markers as the level-name
     axis (the split is *geometry*, not the name).
  2. **`eval_level`** in `cross.rs`: assembles `Trigger::HorizontalCross`, routes
     through the existing `eval_trigger` with an **empty window** (that arm ignores
     it), so `line_price_at` never runs. The proven `Line` projection path is
     byte-identical. `cross_buffer_pct` still applies (a cap won't trip on a graze);
     the retest time-decay tolerance correctly does *not* (a cap is strict).
  3. **`Effect::Invalidate { rule_id }`** + an `Invalidated` fact kind + a
     `PLAN_SCOPE = "__plan__"` sentinel. The driver stamps `(PLAN_SCOPE,
     invalidated)=At(now)` on apply **and** folds the effect into the returned
     list. NOT `latest_bar`-gated (invalidation is timeless).
  4. **`Invalidate<'r, L: LineName>`** rule mirroring `BreakAndClose`
     (PhantomData over its cap, plan-scoped fire-once, `level_typed::<L>()`,
     `eval_level`, spread-hour gate). Two `RuleKind`s — `InvalidateHigh`→`TooHigh`,
     `InvalidateLow`→`TooLow` — so the cap is fixed by the kind, never a runtime
     string (keeps the 4b invariant). `StopNextEntry`-only: retire blocks the
     enter, never closes a position.
  5. **Enter second fire-once guard**: reads `(PLAN_SCOPE, invalidated)`; a retired
     plan blocks the enter. End-to-end test: cap cross → `eval_level` →
     `Effect::Invalidate` → driver retire → enter blocked on the same bar.

  Design decisions taken via review (AskUserQuestion): separate `levels` Vec (not
  a `geometry` enum); build the `Invalidate` rule now (not model-only); reach
  `cross.rs` via a `HorizontalCross` `Trigger`; explicit `Effect::Invalidate`
  variant (not a bare fact write); two `RuleKind`s (not one kind + a runtime level
  ref). The `Line` projection path and its tests were left untouched.
- **4d — split `TimeMarker` out of `Line`. DONE** (`993c933`→`4527b9f`).
  `Expiry` became a `TimeMarker { name, at_epoch }` with the `candle.time >=
  marker` path — and, keeping the 4c lesson (don't ship a dead type), a working
  `Expiry` **rule** consumes it. 3 reviewed/green steps:
  1. **`TimeMarker` model** in types-v2: `TradePlan.markers` (`#[serde(default)]`),
     `marker`/`marker_typed::<L>()` mirroring the line/level pairs; new `Expiry`
     `LineName` marker.
  2. **`eval_time`** in `cross.rs`: `candle.time.timestamp() >= marker_epoch`, the
     v1 `TimeReached` arm. Reads the **bar's** time (v1 spine `trade-expiry` fires
     on the bar whose open passes expiry), NOT the tick's `now` (which only v1's
     *control* `TimeReached` — pause/news, not built in engine-v2 — uses).
  3. **`Expiry<'r, L>`** rule + `RuleKind::Expiry`: fire-once on the retire fact,
     `marker_typed::<L>()`, `eval_time`, emits **`Effect::Invalidate`** — reusing
     4c's retire wholesale (expiry *is* a retirement). Simpler than the caps: no
     `last_close`, no spread-hour gate (a time check has neither). Enter unchanged
     (already blocked by the retire fact). End-to-end test: expiry blocks the
     enter on the bar it expired.

  Decision (AskUserQuestion): `TimeMarker` + a `trade-expiry` rule (not model-only,
  not the full news-windows slice). **`pause`/`resume` + `news-start`/`news-end`
  deferred** — they need a news-window / pause state concept engine-v2's fact
  blackboard doesn't model yet (v1's `open_news_windows`/`PlanState`); that's its
  own design slice (how a news window lives as facts, how the enter/close gate on
  it). Build when the news rules land.

## Risks / watch-points

- **`cross.rs` is proven + historically buggy** (bar-index projection,
  `[[trendline_crosses_bar_index_not_wallclock]]`). 4c must NOT change the `Line`
  projection path — only give `PriceLevel` its own simpler path. Add fresh tests
  for the `PriceLevel` path; leave the `Line` tests untouched.
- **Retest tolerance / spread-hour gates** live in the rules, not the geometry —
  the split must not disturb them (4a–4b are naming; 4c changes only which cross
  fn a level takes).
- **Serialization stays string-on-the-wire.** A round-trip test (plan →
  serialize → deserialize → same typed facts) guards the boundary in 4b.
```
