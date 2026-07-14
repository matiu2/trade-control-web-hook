//! The v2 **fact-based, per-line-generic** plan model.
//!
//! This is a clean-slate rewrite of the plan shape (see
//! `SCOPING-rule-based-engine.md`). Unlike v1's [`trade_control_core::trade_plan::TradePlan`]
//! — which carries a `Phase`-driven sequential spine of [`ConditionRule`]s each
//! with its own inline `Trigger` geometry — a v2 [`TradePlan`] separates
//! **lines** (named geometry) from **rules** (behaviour that *references* a line
//! by name). Facts are keyed by that line name, so break-and-close, retest, and
//! enter meet on `(line, kind)` without a central state machine.
//!
//! # The `Rule` name split
//!
//! There are two "rule" concepts and they must not collide:
//!
//! - [`PlanRule`] (here) — **plan data**: serializable, part of the wire format
//!   `tv-arm-v2` will bake. It *describes* a rule (which line, which kind, what
//!   to fire).
//! - [`Rule`](crate::rule::Rule) — **behaviour**: the trait the driver ticks.
//!
//! We keep the trait named `Rule` (it's the verb-y, behavioural thing the whole
//! engine is *about*) and call the data struct `PlanRule` (it's "a rule *in the
//! plan*"). A `PlanRule` is turned into a boxed `dyn Rule` by the driver.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use trade_control_core::broker::Granularity;
use trade_control_core::intent::{Direction, Intent};
use trade_control_core::trade_plan::{BarEvent, CrossDir, LinePoint};

/// An enter rule's precondition map: for each named line, the **ordered** list of
/// milestone fact-kinds that must have stamped on it, earliest first.
///
/// `{ "neckline": ["break_close", "retest"] }` means: before this enter may
/// place, `(neckline, break_close)` must be set AND `(neckline, retest)` must be
/// set with a **strictly later** fact-time. Lines are independent (no cross-line
/// ordering); ALL lines must be satisfied. Milestones are fact-**kind** strings,
/// not rule ids — the enter references the facts, decoupled from which rule wrote
/// them (see [`engine_v2_enter_preps_layered`] and `SCOPING-rule-based-engine.md`).
///
/// A `BTreeMap` (not `HashMap`) for a stable, deterministic iteration order — the
/// satisfaction check must be order-independent across lines, and deterministic
/// iteration keeps tests and any future logging reproducible.
pub type PrepMap = BTreeMap<String, Vec<String>>;

/// How an [`RuleKind::Enter`] places its order — the entry **mechanism**, an axis
/// orthogonal to its [`PrepMap`] preconditions (stop/limit/market = *how* to
/// place; preps = *what must hold first*). One `Enter` kind × this field replaces
/// a combinatorial `StopEntry`/`LimitEntry`/… enum (see
/// `SCOPING-rule-based-engine.md`, "`RuleKind` is the fork for entry").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryMechanism {
    /// A resting stop order above (long) / below (short) the trigger — fills only
    /// if price trades *through* it. The first mechanism built (slice: stop-entry).
    Stop,
    /// A resting limit order — fills at or better than the trigger. Not yet built.
    Limit,
    /// A market order at the current price. Not yet built.
    Market,
}

/// One named line — the geometric substrate a [`PlanRule`] references.
///
/// `a`/`b` are the two (time, price) anchors, reusing v1's [`LinePoint`] (it's
/// exactly `{ at_epoch, price }`, which is all a v2 line needs). A horizontal
/// line is expressed with `a.price == b.price`; a sloped neckline has distinct
/// prices. A line is always evaluated extend-forward (crosses past the second
/// anchor's bar-index count) — a neckline always projects forward, so there is
/// no per-line flag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    /// The line's name, e.g. `"neckline"`. Facts are keyed by this.
    pub name: String,
    /// First anchor (time, price).
    pub a: LinePoint,
    /// Second anchor (time, price).
    pub b: LinePoint,
}

/// The behaviour class of a [`PlanRule`]. Starts with the one variant slice-1
/// needs and grows as rules land (retest, enter, invalidation, …). Deliberately
/// a small closed enum, not v1's `Trigger`/`RuleKind` pair — a v2 rule's *cross
/// mechanics* come from its `bar`/`dir` + the referenced [`Line`], and its
/// *role* is this `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    /// Break-and-close on a line — the first prep. Writes the
    /// `(line, "break_close")` fact on a genuine close through the line.
    BreakAndClose,
    /// The retest of a line after its break-and-close. Reads the
    /// `(line, "break_close")` fact (does nothing until it is set) and, on a
    /// genuine retest cross strictly after the break, writes the
    /// `(line, "retest")` fact. The first fact *consumer* in v2 — it gates on a
    /// fact another rule produced.
    Retest,
    /// The entry. Reads the enter's [`PlanRule::preps`] map — for every named
    /// line, all listed milestone facts must be set and their times strictly
    /// increasing in list order — and, only when all lines are satisfied, emits
    /// an acquisitive [`Effect::PlaceOrder`](crate::effect::Effect::PlaceOrder).
    /// The *mechanism* (stop/limit/market) is the separate
    /// [`PlanRule::mechanism`] field. The first rule to produce a broker effect.
    Enter,
}

/// A rule as **plan data** — it references a [`Line`] by name, says how the
/// cross is tested (`bar`/`dir`), what role it plays (`kind`), and what
/// [`Intent`] to fire. See the module doc for why this is `PlanRule`, not
/// `Rule`.
// No `PartialEq`: `Intent` (below) does not implement it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRule {
    /// Stable id for this rule (e.g. `03-prep-break-and-close`). Used for
    /// attribution and as the [`FiredIntent`](trade_control_core::plan_eval::FiredIntent)
    /// `rule_id`.
    pub id: String,
    /// The [`Line::name`] this rule tests its cross against.
    pub line: String,
    /// The rule's behaviour class.
    pub kind: RuleKind,
    /// The intent this rule dispatches when it fires.
    pub intent: Intent,
    /// *When within a bar* the cross is tested (close vs intrabar range).
    pub bar: BarEvent,
    /// Which direction through the line counts as a cross.
    pub dir: CrossDir,
    /// **Enter-only** precondition map (see [`PrepMap`]). Empty for non-enter
    /// rules and for a no-prep enter. `#[serde(default)]` so prep rules — and
    /// pre-preps plans on the wire — deserialize with an empty map.
    #[serde(default)]
    pub preps: PrepMap,
    /// **Enter-only** entry mechanism (see [`EntryMechanism`]). Ignored by
    /// non-enter rules. Defaults to [`EntryMechanism::Stop`] (the first — and, in
    /// this slice, only — mechanism built), so non-enter rules and older wire
    /// plans deserialize without it.
    #[serde(default = "default_mechanism")]
    pub mechanism: EntryMechanism,
}

/// Serde default for [`PlanRule::mechanism`] — [`EntryMechanism::Stop`], the
/// mechanism this slice builds. Non-enter rules carry it harmlessly.
fn default_mechanism() -> EntryMechanism {
    EntryMechanism::Stop
}

/// A v2 trade plan — lines + rules, per-line-generic, no phase.
///
/// The driver ticks each [`PlanRule`] against the candle stream; rules
/// communicate only through [`Facts`](crate::facts::Facts), keyed by the line
/// names declared here. There is no `Phase` and no ordering intelligence — rule
/// order is the list order `tv-arm-v2` bakes.
// No `PartialEq`: carries `PlanRule`s whose `Intent` doesn't implement it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradePlan {
    /// The trade this plan drives. Facts are implicitly scoped to it (one
    /// [`Facts`](crate::facts::Facts) per plan).
    pub trade_id: String,
    /// Canonical instrument (e.g. `EURUSD`). Passed to the spread-hour gate.
    pub instrument: String,
    /// Trade direction.
    pub direction: Direction,
    /// Chart granularity — used to resolve a trendline's `bar_seconds` fallback
    /// divisor when an anchor predates the fetched window.
    pub granularity: Granularity,
    /// The named lines rules reference.
    pub lines: Vec<Line>,
    /// The rules, in fire order.
    pub rules: Vec<PlanRule>,
    /// Plan-level cross-depth buffer as a percentage of the line price — a
    /// directional cross must pierce this far past the line to count (a one-tick
    /// graze doesn't). `0.0` reproduces the bare line.
    pub cross_buffer_pct: f64,
    /// Near-side tolerance step for the retest: a retest's closeness tolerance
    /// is `(N - 1) × retest_atr_step × ATR`, where `N` is the number of bars
    /// after the break-and-close (first = 1 → tolerance 0, must reach the line).
    /// Default [`trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP`]
    /// (0.075). See [`RuleKind::Retest`].
    pub retest_atr_step: f64,
}

impl TradePlan {
    /// Look up a line by name.
    pub fn line(&self, name: &str) -> Option<&Line> {
        self.lines.iter().find(|l| l.name == name)
    }
}
