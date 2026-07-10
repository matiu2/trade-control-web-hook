//! The serialisable **trade plan** — one signed bundle that captures every
//! condition `tv-arm` used to encode as a separate TradingView alert.
//!
//! # Why this exists
//!
//! Today `tv-arm` reads a hand-drawn chart and creates ~5–15 TradingView alerts
//! per trade; each alert evaluates one condition *on TV's servers* and POSTs an
//! already-fired [`Intent`](crate::intent::Intent) to the worker. The
//! server-side engine inverts that: `tv-arm` folds **all** of a trade's
//! conditions into one [`TradePlan`], signs it, and registers it
//! ([`Action::Register`](crate::intent::Action::Register)). The engine then
//! polls broker candles on a cron tick, evaluates each [`ConditionRule`]
//! itself, and dispatches the embedded [`Intent`] when a rule fires — the same
//! intent the TV alert would have POSTed.
//!
//! This module is the **data model only** (it lives in `core` because the
//! [`Intent`] needs to hold a plan, and `core` can't depend on the engine
//! crate). The pure evaluator that consumes a plan + new candles + prior state
//! lives in the engine crate (Stage D); it generalises
//! [`plan_mw_update`](crate::intent::plan_mw_update).
//!
//! # The `Frequency` split (server-side is stateful)
//!
//! A TradingView `OnFirstFire` alert re-fires every time price touches the
//! line, because TV's alert model is per-bar and stateless. The engine holds
//! per-rule fired flags, so a fired rule **latches** — once a retest prep has
//! fired and been recorded, the engine stops evaluating that rule. TV's single
//! `Frequency` enum therefore conflated two concerns the engine keeps separate:
//!
//! - [`BarEvent`] — *when within a bar* the condition is tested: [`Intrabar`]
//!   (any high/low touch, recovered by the candle high/low lookback) vs
//!   [`OnClose`] (the close price only).
//! - [`FireMode`] — *how many times* the rule may fire: [`Once`] (latch after
//!   the first fire — preps, single-shot vetos) vs [`EveryBar`] (re-evaluated
//!   every tick — the M/W heartbeat that recomputes geometry each bar).
//!
//! [`Intrabar`]: BarEvent::Intrabar
//! [`OnClose`]: BarEvent::OnClose
//! [`Once`]: FireMode::Once
//! [`EveryBar`]: FireMode::EveryBar

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
pub use trade_control_conventions::RuleKind;

use crate::broker::Granularity;
use crate::intent::{Direction, Intent};

/// Default cross-depth buffer (percent of the crossed level's price) baked onto
/// a plan at arm time. **`0.02%`** — calibrated on the AUD/JPY iH&S of
/// 2026-06-29: a buffer sweep showed the trade is a **−1.43R** loss with no
/// buffer (three shallow early retest taps each stop out before the runner),
/// flips to **+0.57R net** at `0.02%` (the shallow taps are filtered, leaving
/// only the entry that runs to TP), holds through `~0.07%`, and over-tightens
/// into a starved 0-trade plan at `0.1%`. `0.02%` is the threshold where the
/// trade turns profitable, so it is the default. Set a different value per-trade
/// to tune (a future `tv-arm --cross-buffer-pct` flag overrides it); `0.0`
/// restores the bare wick-touch behaviour. See [`TradePlan::cross_buffer_pct`].
pub const DEFAULT_CROSS_BUFFER_PCT: f64 = 0.02;

/// Default per-bar decay step (in ATR multiples) for the retest tolerance —
/// **0.075**. The first bar after the break must reach the neckline; each later
/// bar loosens by `0.075 × ATR`, so the retest accepts a wick within ~1 ATR of
/// the line by ~bar 14. Chosen by the operator as a starting point for visual
/// tuning; override per-trade with `tv-arm --retest-atr-step`. See
/// [`TradePlan::retest_atr_step`].
pub const DEFAULT_RETEST_ATR_STEP: f64 = 0.075;

fn default_retest_atr_step() -> f64 {
    DEFAULT_RETEST_ATR_STEP
}

/// One signed trade, folded from every alert `tv-arm` would have created. The
/// engine evaluates its [`rules`](Self::rules) against fresh candles each tick.
///
/// Carried inside an [`Intent`](crate::intent::Intent) on an
/// [`Action::Register`](crate::intent::Action::Register); signed as part of the
/// whole-body HMAC like any other intent field, so the plan can't be tampered.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TradePlan {
    /// Mints alongside the trade; ties every rule's fired-state and the plan's
    /// KV row together. Matches the `trade_id` the embedded intents carry.
    pub trade_id: String,
    /// Canonical instrument the engine fetches candles for (e.g. `EUR_USD`).
    pub instrument: String,
    /// Trade direction — long (W / H&S-long) or short (M / H&S-short).
    pub direction: Direction,
    /// Timeframe the trade is armed on; the granularity the engine fetches and
    /// the bar size every `OnClose` rule closes on.
    pub granularity: Granularity,
    /// Instrument pip size baked at arm time (`asset.pip_size` from
    /// `instrument-lookup`). Same value the embedded intents carry; here so the
    /// engine can scale a pip-expressed trigger level without re-deriving it.
    pub pip_size: f64,
    /// The conditions to evaluate. Order is informational only — the engine
    /// evaluates every not-yet-latched rule each tick.
    pub rules: Vec<ConditionRule>,
    /// Observe-only mode. When `true`, the engine evaluates the plan and
    /// advances its [`PlanState`](crate::plan_state::PlanState) exactly as a
    /// live plan, but **does not dispatch** fired intents to the broker — each
    /// fire is logged as a `SHADOW would-fire` line instead. This is the safe
    /// way to run the engine alongside the live TradingView alerts on demo (the
    /// Stage F gate): both observe the same candles, but only the TV alert
    /// places real orders, so the two can be diffed without double-firing.
    ///
    /// Signed as part of the plan (it rides the whole-body HMAC), so a plan's
    /// shadow/live status can only be set at arm time, not flipped in flight.
    /// `#[serde(default)]` so plans registered before this field existed
    /// deserialize as **live** (`false`).
    #[serde(default)]
    pub shadow: bool,
    /// Cross-depth buffer, as a **percent of the crossed level's price**, that
    /// an *intrabar* directional cross must pierce past the line before it
    /// counts. Guards against a one-tick graze tripping a retest / invalidation:
    /// a `Down` cross needs `low <= level - (pct/100)*level`, an `Up` cross needs
    /// `high >= level + (pct/100)*level`. `Either` keeps a bare straddle, and
    /// `OnClose` (break-and-close) is unaffected — its close must already be on
    /// the far side. `0.0` (the default) reproduces the pre-buffer behaviour, so
    /// plans signed before this field deserialize unchanged.
    ///
    /// Plan-level (uniform across the plan's crosses) and signed as part of the
    /// whole-body HMAC, so it's fixed at arm time. A future `tv-arm` flag can
    /// override the arm-time default.
    #[serde(default)]
    pub cross_buffer_pct: f64,
    /// Per-bar decay step (in **ATR multiples**) for the retest's
    /// closeness-to-neckline tolerance. The retest cross loosens as bars pass
    /// since the break-and-close: the first bar after the break must actually
    /// *reach* the neckline (tolerance 0), and each subsequent bar adds
    /// `retest_atr_step × ATR` of **near-side** slack, so a wick that comes
    /// *within* the tolerance of the line (without reaching it) still stamps the
    /// retest. With `N` = bars since break-and-close (first = 1) and `ATR` the
    /// Wilder ATR at the current bar, the tolerance is `(N-1) × retest_atr_step ×
    /// ATR`. The rationale (operator): a retest right after the break should be
    /// tight, but the further price drifts in time the less its exact distance to
    /// the neckline matters.
    ///
    /// Only the retest rule uses this; every other intrabar cross keeps
    /// [`Self::cross_buffer_pct`] (which tightens, not loosens). Signed as part of
    /// the whole-body HMAC, so it's fixed at arm time; `tv-arm --retest-atr-step`
    /// overrides the default. `#[serde(default = …)]` gives plans signed before
    /// this field the same **0.075** default rather than a silent `0.0` (which
    /// would freeze the retest at "must reach" forever).
    #[serde(default = "default_retest_atr_step")]
    pub retest_atr_step: f64,
    /// Arm-time replay cursor (`tv-arm --start`, a Unix second), baked so the
    /// offline `replay-candles` harness can derive a self-consistent window
    /// without reading the TradingView chart's replay cursor. The worker does
    /// **not** act on this — it's a journaling aid. `replay-candles` uses it as
    /// the start cursor (its own `--start` flag still overrides). `None` when
    /// `tv-arm` was run without `--start`; `#[serde(skip_serializing_if)]` keeps
    /// it out of the JSON entirely then, so pre-field plans round-trip unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_start: Option<i64>,
    /// Wall-clock instant this plan was armed by `tv-arm` (`Utc::now()` at
    /// arm time). Baked so the arming datetime can be read back later from
    /// the plan — a journaling aid only. The worker/engine does **not** act
    /// on it (it never gates or schedules off `armed_at`). `None` for plans
    /// registered before this field existed; `#[serde(skip_serializing_if)]`
    /// keeps it out of the JSON entirely then, so pre-field plans round-trip
    /// unchanged. Nested inside the whole-`TradePlan` signed line (like
    /// `replay_start`), so it adds no new top-level signed key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed_at: Option<DateTime<Utc>>,
}

/// One condition + the intent it fires. The engine evaluates [`trigger`] each
/// tick subject to [`fire_mode`], and on a fire dispatches [`intent`] through
/// the same path the webhook uses.
///
/// [`trigger`]: Self::trigger
/// [`fire_mode`]: Self::fire_mode
/// [`intent`]: Self::intent
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConditionRule {
    /// Stable id within the plan (e.g. `04-prep-retest`). Keys this rule's
    /// fired-flag in the plan's state and labels engine log lines.
    pub rule_id: String,
    /// What price/time/pattern condition fires this rule.
    pub trigger: Trigger,
    /// How many times the rule may fire (latch-once vs re-evaluate every bar).
    pub fire_mode: FireMode,
    /// The fully-formed intent dispatched when the trigger fires — the same
    /// signed action the TV alert would have POSTed (an `enter`, a `veto`, a
    /// `prep`, a `close`, etc.).
    pub intent: Intent,
    /// The rule's behaviour class in the engine spine, resolved once at arm time
    /// from its basename (see [`RuleKind`]). The engine reads this instead of
    /// re-deriving "what kind of guard is this?" from `rule_id`/`Action` in six
    /// places (the seam behind the v73 pre-break-arming bug).
    ///
    /// `#[serde(default)]` → plans signed before this field deserialize as
    /// [`RuleKind::Unspecified`] and round-trip byte-identically (the field is
    /// nested inside the whole-`TradePlan` signed line, not a new top-level
    /// signed key — same as `cross_buffer_pct` / `pip_size` / `tick_size`). The
    /// engine treats `Unspecified` as "derive the old way" during the migration
    /// window, so an absent kind never mis-classifies an in-flight plan.
    #[serde(default)]
    pub kind: RuleKind,
}

/// What fires a [`ConditionRule`]. Each variant maps 1:1 from a TradingView
/// alert shape `tv-arm` used to create (see the port table in the engine docs):
/// `HorizontalCross`/`PriceValueCross` ⇐ the `Drawing`(horz)/`PriceValue`
/// alerts, `TrendlineCross` ⇐ the `Drawing`(trendline) alerts, `TimeReached`
/// ⇐ the vertical-line / `VertLineAt` alerts, `MwEveryBar` + `PinePattern`
/// ⇐ the `PineAlertcondition` alerts.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    /// Price crosses a fixed horizontal level (`01-veto-too-high/low` on a
    /// horizontal invalidation line). The `level` is an absolute MID price.
    HorizontalCross {
        level: f64,
        dir: CrossDir,
        bar: BarEvent,
    },
    /// Price reaches a computed numeric level with no drawing on the chart —
    /// the pcl-exhausted veto and the M/W cancel / abort / overshoot levels.
    /// `level` is an absolute MID price baked at arm time.
    PriceValueCross {
        level: f64,
        dir: CrossDir,
        bar: BarEvent,
    },
    /// Price crosses a sloped line (`03-prep-break-and-close`,
    /// `04-prep-retest` on neckline trendlines). The line is two
    /// (time, price) anchors; the engine interpolates the level at each
    /// candle's time and tests the cross there.
    TrendlineCross {
        a: LinePoint,
        b: LinePoint,
        /// Whether to evaluate crossings past the second anchor (the engine's
        /// analogue of the TV `extend_forward` payload flag — see the README's
        /// trendline note). Almost always `true` for a neckline.
        extend_forward: bool,
        /// Nominal duration of one bar in seconds (the chart granularity baked
        /// at arm time, e.g. 3600 for H1). The engine interpolates the line's
        /// level in **bar-index** space — not wall-clock — so that a trendline
        /// advances one step *per traded bar* and not per elapsed second
        /// (TradingView's x-axis is ordinal; closed sessions aren't plotted, so
        /// nights/weekends collapse to a single bar step). The engine prefers to
        /// count the *actual* bars present in the broker feed between the anchors
        /// (gaps are absent from the feed — confirmed on ALPHABET: a US stock's
        /// 18h overnight and 66h weekend gaps each collapse to one bar). This
        /// `bar_seconds` is the **fallback divisor** used only when an anchor
        /// predates the fetched candle window, where no bar-count is available.
        /// `#[serde(default)]` → `0` on plans signed before this field existed,
        /// which the engine treats as "no fallback; pure bar-count only".
        #[serde(default)]
        bar_seconds: i64,
        dir: CrossDir,
        bar: BarEvent,
    },
    /// A wall-clock time is reached (vertical lines: trade-expiry,
    /// prep-expiry, pause/resume, news-start/end). `at` is a Unix epoch in
    /// seconds (UTC). Fires on the first tick whose `now` is at or past it.
    TimeReached { at_epoch: i64 },
    /// The M/W heartbeat — re-evaluate the live geometry every bar close and
    /// let [`plan_mw_update`](crate::intent::plan_mw_update) decide. Always
    /// pairs with [`FireMode::EveryBar`].
    MwEveryBar,
    /// A candle-pattern signal fired (the H&S `05-enter` / `06-close-on-...`
    /// Pine alertconditions). Evaluated by the Rust port of the Pine detector
    /// (Stage E); `pattern` is unset to mean "any of the configured patterns".
    PinePattern {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<crate::intent::SignalKind>,
        dir: Direction,
    },
}

/// One endpoint of a [`Trigger::TrendlineCross`] — a (time, price) anchor.
/// `at_epoch` is Unix seconds (UTC), `price` an absolute MID price.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct LinePoint {
    pub at_epoch: i64,
    pub price: f64,
}

/// Cross direction — which way price must move through the level for the
/// trigger to fire. Mirrors TradingView's `cross` / `cross_up` / `cross_down`
/// ([`ConditionType`] in `tv-arm`), but named for the engine's own evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossDir {
    /// Either direction — touch from above or below (TV `cross`).
    Either,
    /// Price moves up through the level (TV `cross_up`).
    Up,
    /// Price moves down through the level (TV `cross_down`).
    Down,
}

/// *When within a bar* a price condition is tested. Split out from TV's
/// `Frequency` so it composes independently with [`FireMode`]; see the module
/// docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BarEvent {
    /// Test against the bar's full range — any wick high/low touch counts.
    /// Recovered retroactively by the candle high/low lookback (TV
    /// `OnFirstFire` price alerts map here).
    Intrabar,
    /// Test the close price only (TV `OnBarClose` price alerts map here).
    OnClose,
}

/// *How many times* a rule may fire. Split out from TV's `Frequency`; see the
/// module docs. A TV `OnFirstFire` alert was [`Intrabar`](BarEvent::Intrabar) +
/// [`Once`](Self::Once); the M/W heartbeat is [`OnClose`](BarEvent::OnClose) +
/// [`EveryBar`](Self::EveryBar).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FireMode {
    /// Fire at most once, then latch — the engine stops evaluating the rule.
    /// Preps and single-shot vetos. The server-side improvement over a TV
    /// alert that re-fires on every touch.
    Once,
    /// Re-evaluate every tick; never latches. The M/W geometry heartbeat.
    EveryBar,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trigger round-trips through YAML with its `kind` tag, and the
    /// `BarEvent`/`FireMode` split serialises to the snake-case wire form the
    /// plan builder and engine will share.
    #[test]
    fn trigger_yaml_round_trip_tags_kind() {
        let t = Trigger::HorizontalCross {
            level: 1.2345,
            dir: CrossDir::Up,
            bar: BarEvent::Intrabar,
        };
        let yaml = serde_yaml::to_string(&t).unwrap();
        assert!(yaml.contains("type: horizontal_cross"), "got: {yaml}");
        assert!(yaml.contains("dir: up"), "got: {yaml}");
        assert!(yaml.contains("bar: intrabar"), "got: {yaml}");
        let back: Trigger = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn fire_mode_and_bar_event_wire_form() {
        assert_eq!(
            serde_yaml::to_string(&FireMode::Once).unwrap().trim(),
            "once"
        );
        assert_eq!(
            serde_yaml::to_string(&FireMode::EveryBar).unwrap().trim(),
            "every_bar"
        );
        assert_eq!(
            serde_yaml::to_string(&BarEvent::OnClose).unwrap().trim(),
            "on_close"
        );
    }

    #[test]
    fn mw_every_bar_trigger_has_no_payload() {
        let yaml = serde_yaml::to_string(&Trigger::MwEveryBar).unwrap();
        assert!(yaml.contains("type: mw_every_bar"), "got: {yaml}");
        let back: Trigger = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, Trigger::MwEveryBar);
    }

    #[test]
    fn pine_pattern_kind_optional() {
        let any = Trigger::PinePattern {
            pattern: None,
            dir: Direction::Short,
        };
        let yaml = serde_yaml::to_string(&any).unwrap();
        // `pattern: None` is elided so the wire form stays minimal.
        assert!(
            !yaml.contains("pattern:"),
            "pattern should be elided: {yaml}"
        );
        assert!(yaml.contains("dir: short"), "got: {yaml}");
        let back: Trigger = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, any);
    }

    #[test]
    fn trendline_point_round_trips() {
        let t = Trigger::TrendlineCross {
            a: LinePoint {
                at_epoch: 1_700_000_000,
                price: 1.10,
            },
            b: LinePoint {
                at_epoch: 1_700_003_600,
                price: 1.11,
            },
            extend_forward: true,
            bar_seconds: 3600,
            dir: CrossDir::Down,
            bar: BarEvent::OnClose,
        };
        let back: Trigger = serde_yaml::from_str(&serde_yaml::to_string(&t).unwrap()).unwrap();
        assert_eq!(back, t);
    }

    /// A trendline signed before `bar_seconds` existed deserializes with `0`
    /// (the "pure bar-count, no fallback" sentinel) rather than failing.
    #[test]
    fn trendline_missing_bar_seconds_defaults_to_zero() {
        let yaml = r#"type: trendline_cross
a: {at_epoch: 100, price: 1.0}
b: {at_epoch: 200, price: 1.5}
extend_forward: true
dir: down
bar: on_close
"#;
        let t: Trigger = serde_yaml::from_str(yaml).unwrap();
        let Trigger::TrendlineCross { bar_seconds, .. } = t else {
            panic!("expected a trendline cross, got {t:?}");
        };
        assert_eq!(bar_seconds, 0, "missing bar_seconds should default to 0");
    }

    /// The `shadow` flag survives a JSON round-trip when set.
    #[test]
    fn shadow_flag_round_trips() {
        let json = r#"{"trade_id":"t-1","instrument":"EUR_USD","direction":"short",
            "granularity":"h1","pip_size":0.0001,"rules":[],"shadow":true}"#;
        let plan: TradePlan = serde_json::from_str(json).unwrap();
        assert!(plan.shadow);
        let back: TradePlan = serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert!(back.shadow, "shadow flag should survive a round-trip");
    }

    /// A plan registered before the `shadow` field existed (no `shadow` key in
    /// the wire body) must deserialize as **live** — `#[serde(default)]` → false.
    #[test]
    fn missing_shadow_defaults_to_live() {
        let json = r#"{"trade_id":"t-1","instrument":"EUR_USD","direction":"short",
            "granularity":"h1","pip_size":0.0001,"rules":[]}"#;
        let plan: TradePlan = serde_json::from_str(json).unwrap();
        assert!(
            !plan.shadow,
            "absent shadow key must default to live (false)"
        );
    }

    /// A plan signed before `cross_buffer_pct` existed (no key in the wire body)
    /// must deserialize with a `0.0` buffer — `#[serde(default)]` → the
    /// pre-buffer bare-wick behaviour, so old plans are byte-for-byte unchanged.
    #[test]
    fn missing_cross_buffer_pct_defaults_to_zero() {
        let json = r#"{"trade_id":"t-1","instrument":"EUR_USD","direction":"short",
            "granularity":"h1","pip_size":0.0001,"rules":[]}"#;
        let plan: TradePlan = serde_json::from_str(json).unwrap();
        assert_eq!(
            plan.cross_buffer_pct, 0.0,
            "absent cross_buffer_pct must default to 0.0 (no buffer)"
        );
    }

    /// The `cross_buffer_pct` value survives a JSON round-trip when set.
    #[test]
    fn cross_buffer_pct_round_trips() {
        let json = r#"{"trade_id":"t-1","instrument":"EUR_USD","direction":"short",
            "granularity":"h1","pip_size":0.0001,"rules":[],"cross_buffer_pct":0.1}"#;
        let plan: TradePlan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.cross_buffer_pct, 0.1);
        let back: TradePlan = serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert_eq!(back.cross_buffer_pct, 0.1, "must survive a round-trip");
    }

    /// A plan with no `replay_start` deserializes to `None`, and re-serializing
    /// omits the field entirely (so pre-field plans round-trip byte-clean).
    #[test]
    fn missing_replay_start_defaults_to_none_and_is_omitted() {
        let json = r#"{"trade_id":"t-1","instrument":"EUR_USD","direction":"short",
            "granularity":"h1","pip_size":0.0001,"rules":[]}"#;
        let plan: TradePlan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.replay_start, None);
        let out = serde_json::to_string(&plan).unwrap();
        assert!(
            !out.contains("replay_start"),
            "None replay_start must be skipped in the JSON, got: {out}"
        );
    }

    /// A baked `replay_start` (from `tv-arm --start`) survives a round-trip.
    #[test]
    fn replay_start_round_trips() {
        let json = r#"{"trade_id":"t-1","instrument":"EUR_USD","direction":"short",
            "granularity":"h1","pip_size":0.0001,"rules":[],"replay_start":1781208000}"#;
        let plan: TradePlan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.replay_start, Some(1781208000));
        let back: TradePlan = serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert_eq!(
            back.replay_start,
            Some(1781208000),
            "must survive a round-trip"
        );
    }

    /// A plan with no `armed_at` deserializes to `None` and re-serializes
    /// without the key, so plans registered before the field round-trip clean.
    #[test]
    fn missing_armed_at_defaults_to_none_and_is_omitted() {
        let json = r#"{"trade_id":"t-1","instrument":"EUR_USD","direction":"short",
            "granularity":"h1","pip_size":0.0001,"rules":[]}"#;
        let plan: TradePlan = serde_json::from_str(json).unwrap();
        assert_eq!(plan.armed_at, None);
        let out = serde_json::to_string(&plan).unwrap();
        assert!(
            !out.contains("armed_at"),
            "None armed_at must be skipped in the JSON, got: {out}"
        );
    }

    /// A baked `armed_at` (the arm-time datetime `tv-arm` records) survives a
    /// round-trip.
    #[test]
    fn armed_at_round_trips() {
        let json = r#"{"trade_id":"t-1","instrument":"EUR_USD","direction":"short",
            "granularity":"h1","pip_size":0.0001,"rules":[],
            "armed_at":"2026-05-01T09:30:00Z"}"#;
        let plan: TradePlan = serde_json::from_str(json).unwrap();
        let expected = "2026-05-01T09:30:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(plan.armed_at, Some(expected));
        let back: TradePlan = serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert_eq!(back.armed_at, Some(expected), "must survive a round-trip");
    }

    /// A rule signed before `kind` existed (no `kind` key) deserializes as
    /// `Unspecified` — never a real kind — so the engine falls back to legacy
    /// derivation and an old plan's `too-high` isn't silently mis-classified.
    #[test]
    fn missing_rule_kind_defaults_to_unspecified() {
        let json = r#"{"rule_id":"01-veto-too-high",
            "trigger":{"type":"horizontal_cross","level":1.2,"dir":"up","bar":"on_close"},
            "fire_mode":"once",
            "intent":{"v":1,"action":"veto","instrument":"EUR_USD","id":"veto-1",
                "not_after":"2026-05-13T20:00:00Z"}}"#;
        let rule: ConditionRule = serde_json::from_str(json).unwrap();
        assert_eq!(
            rule.kind,
            RuleKind::Unspecified,
            "absent kind must default to Unspecified, not a real class"
        );
    }

    /// A stamped `kind` survives a JSON round-trip in its snake_case wire form.
    #[test]
    fn rule_kind_round_trips() {
        let json = r#"{"rule_id":"01-veto-too-high",
            "trigger":{"type":"horizontal_cross","level":1.2,"dir":"up","bar":"on_close"},
            "fire_mode":"once",
            "intent":{"v":1,"action":"veto","instrument":"EUR_USD","id":"veto-1",
                "not_after":"2026-05-13T20:00:00Z"},
            "kind":"setup_invalidation"}"#;
        let rule: ConditionRule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.kind, RuleKind::SetupInvalidation);
        let back: ConditionRule =
            serde_json::from_str(&serde_json::to_string(&rule).unwrap()).unwrap();
        assert_eq!(
            back.kind,
            RuleKind::SetupInvalidation,
            "kind must survive a round-trip"
        );
    }
}
