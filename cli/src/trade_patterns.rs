//! `build-trade` — questionnaire-driven multi-alert trade emission.
//!
//! A *trade pattern* (H&S, IH&S, M-top, W-bottom) is a fixed set of 5
//! alerts that share a `trade_id`, a `trade_expiry` anchor, and a single
//! direction. Each alert is independent on the wire — the worker has no
//! notion of "trades" — but the CLI groups them so the operator answers
//! one questionnaire and gets five signed YAMLs to drop into
//! TradingView's alert dialogs.
//!
//! Layout: this file holds the orchestration, the pattern enum, the
//! per-pattern geometry table (entry / SL anchors, invalidation veto
//! name), and one shared questionnaire that branches on the geometry.
//! Adding a new direction-only variant (e.g. M / W) is one new arm in
//! [`PatternGeometry::for_pattern`]; adding a structurally different
//! pattern would need a separate build function. Today H&S and IH&S
//! are wired up — based on the operator's reference templates
//! `short.yaml` and `long.yaml`. M / W are in the picker but the
//! build path emits "not yet implemented" so the picker doesn't lie.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use color_eyre::eyre::{Context, Result, eyre};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{FuzzySelect, Input};
use serde::{Deserialize, Serialize};

use trade_control_conventions::AlertBasename;
use trade_control_core::intent::{
    Action, BlackoutCloseAction, BrokerKind, Direction, EntrySpec, Intent, MW_CANCEL_VETO_NAME,
    MW_OVERSHOOT_VETO_NAME, PriceAnchor, PriceRef, RecoverEntry, RecoverEntryAction, TakeProfit,
    VetoLevel,
};
use trade_control_core::sig::KEY_LEN;

use crate::control::{
    wrap_signed_direct_enter, wrap_signed_template, wrap_signed_template_drawing,
};
use crate::expiry;
use crate::instruments::validate_instrument;

/// Default lifetime of the entry window expressed as a percentage of the
/// span between *now* and `trade_expiry`. 80% means: if the trade is
/// valid for 5 days, entries fire only in the first 4. After that, even
/// a textbook signal is too late to be worth taking.
const DEFAULT_ENTRY_DEADLINE_PCT: u32 = 80;

/// Default grace tail on every veto past `trade_expiry`. Stops a veto
/// from lapsing the same instant as `trade_expiry` itself and letting a
/// late retry sneak in on a clock skew.
const DEFAULT_POST_EXPIRY_GRACE: Duration = Duration::minutes(30);

/// Default risk percent prompt. Tuned conservatively; the operator
/// overrides at the prompt.
const DEFAULT_RISK_PCT: f64 = 1.0;

/// The catalogue of supported trade patterns. The discriminant doubles
/// as the CLI argument (`hs`, `ihs`, `m`, `w`) and the label in the
/// fuzzy picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TradePattern {
    /// Head & Shoulders — short.
    Hs,
    /// Inverse Head & Shoulders — long.
    Ihs,
    /// M-top — short.
    M,
    /// W-bottom — long.
    W,
}

impl TradePattern {
    pub fn label(self) -> &'static str {
        match self {
            Self::Hs => "hs — Head & Shoulders (short)",
            Self::Ihs => "ihs — Inverse Head & Shoulders (long)",
            Self::M => "m — M-top (short)",
            Self::W => "w — W-bottom (long)",
        }
    }

    /// Parse the CLI positional form (`hs`, `ihs`, `m`, `w`). Case-
    /// insensitive for ergonomics — operators frequently mistype.
    pub fn parse_arg(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "hs" => Some(Self::Hs),
            "ihs" => Some(Self::Ihs),
            "m" => Some(Self::M),
            "w" => Some(Self::W),
            _ => None,
        }
    }

    /// Short identifier suitable for embedding in a trade_id slug
    /// (lowercase, no whitespace, no separators). Stays distinct from
    /// other patterns.
    fn slug(self) -> &'static str {
        match self {
            Self::Hs => "hs",
            Self::Ihs => "ihs",
            Self::M => "m",
            Self::W => "w",
        }
    }
}

/// Direction-specific bits of a pattern. Hand-rolled per pattern from
/// the operator's reference templates (`short.yaml`, `long.yaml`) so
/// the build path is a single shared questionnaire that branches only
/// on these values.
///
/// Reading the H&S template `short.yaml`:
///   entry = low + 1 pip (stop-entry — fires when price breaks below
///   the recent low), SL = high + 1 pip, invalidation veto =
///   `too-high` (price runs back up through structure).
///
/// IH&S `long.yaml` mirrors it on the other side:
///   entry = high + 1 pip, SL = low + 1 pip (the template uses `+1`
///   not `-1` — a tight SL that sits inside the candle by 1 pip, so a
///   wick to the low takes you out), invalidation veto = `too-low`.
#[derive(Debug, Clone, Copy)]
struct PatternGeometry {
    direction: Direction,
    /// Where the stop-entry trigger price comes from in the plaintext
    /// shell. Operator overrides the offset at the prompt; this is
    /// just the anchor + default.
    entry_anchor: PriceAnchor,
    entry_offset_default: f64,
    /// Where the SL price comes from. Same: anchor fixed by pattern,
    /// offset operator-overridable.
    sl_anchor: PriceAnchor,
    sl_offset_default: f64,
    /// Name of the invalidation veto for this pattern. `too-high` for
    /// shorts (price running back up past the right shoulder),
    /// `too-low` for longs (price running back down past the right
    /// shoulder of an inverse H&S). This veto is drawing-bound on the
    /// chart side (operator-drawn horizontal at the shoulder).
    invalidation_veto_name: &'static str,
    /// Name of the opposite-direction veto, fired when price has run
    /// most of the way to TP without us in. For a short trade this is
    /// `too-low`; for a long trade `too-high`. Built by the same
    /// `build_invalidation_alert` builder as the invalidation veto, but
    /// it is *not* the same level: this one is `StopNextEntry` (an
    /// entry-gate — "don't open a late entry"), whereas the invalidation
    /// veto is `ClosePositions` (thesis dead → flatten). A pcl breach is
    /// in the trade's favour, so it must never close an open position.
    /// See `BUG-too-low-closes-positions.md`. The chart side binds this
    /// veto to a computed price (pcl-exhausted) rather than a drawing.
    pcl_exhausted_veto_name: &'static str,
}

impl PatternGeometry {
    fn for_pattern(p: TradePattern) -> Self {
        match p {
            // Entry/SL anchor to the *latched* signal extremes, not the
            // triggering candle's own wick, so a confirmation re-fire resolves
            // to the same geometry as the break-candle fire (bug #10 finding A).
            //
            // The offset is applied raw at resolution (`anchor + offset_pips *
            // pip`, no direction flip), so each offset's **sign** must push the
            // level *away from the pattern in the breakout direction*: a
            // `signal_low` anchor takes a negative offset (1 pip below the low),
            // a `signal_high` anchor a positive one (1 pip above the high).
            // A flat `+1.0` on every anchor was wrong for the two low anchors —
            // it placed them 1 pip *above* the low (HS entered too high → tight
            // SL → stop-out; a bar closing on its low resolved `InvalidGeometry`).
            //
            // For a short H&S: sell-stop entry 1 pip *below* the break at
            // signal_low, SL 1 pip *above* the pattern at signal_high.
            TradePattern::Hs => Self {
                direction: Direction::Short,
                entry_anchor: PriceAnchor::SignalLow,
                entry_offset_default: -1.0,
                sl_anchor: PriceAnchor::SignalHigh,
                sl_offset_default: 1.0,
                invalidation_veto_name: "too-high",
                pcl_exhausted_veto_name: "too-low",
            },
            // Inverse H&S long: mirror of the above — buy-stop entry 1 pip
            // *above* the break at signal_high, SL 1 pip *below* the pattern at
            // signal_low.
            TradePattern::Ihs => Self {
                direction: Direction::Long,
                entry_anchor: PriceAnchor::SignalHigh,
                entry_offset_default: 1.0,
                sl_anchor: PriceAnchor::SignalLow,
                sl_offset_default: -1.0,
                invalidation_veto_name: "too-low",
                pcl_exhausted_veto_name: "too-high",
            },
            TradePattern::M | TradePattern::W => {
                // Unreachable at runtime — build_trade_interactive
                // rejects these before geometry is consulted.
                unreachable!("geometry for {p:?} not configured")
            }
        }
    }
}

/// Pick a pattern interactively when the positional arg is omitted.
pub fn pick_pattern_interactive() -> Result<TradePattern> {
    let patterns = [
        TradePattern::Hs,
        TradePattern::Ihs,
        TradePattern::M,
        TradePattern::W,
    ];
    let labels: Vec<&str> = patterns.iter().map(|p| p.label()).collect();
    let theme = ColorfulTheme::default();
    let idx = FuzzySelect::with_theme(&theme)
        .with_prompt("pattern (type to filter)")
        .items(&labels)
        .default(0)
        .interact()
        .map_err(|e| eyre!("pattern pick aborted: {e}"))?;
    Ok(patterns[idx])
}

/// One alert ready to be written to disk. Carries the file basename (no
/// extension) so the caller can drop it into the output directory and
/// also reference it from the manifest.
#[derive(Debug)]
pub struct BuiltAlert {
    pub basename: String,
    /// Human-readable purpose for the manifest. e.g. "veto: too-high
    /// (close-positions)". Operators see this when wiring up TV.
    pub purpose: String,
    pub intent: Intent,
}

/// Outputs of a build-trade run, before they're flushed to disk.
#[derive(Debug)]
pub struct BuiltTrade {
    pub trade_id: String,
    pub instrument: String,
    pub trade_expiry: DateTime<Utc>,
    pub alerts: Vec<BuiltAlert>,
    /// The spec used to build this trade — captured so the caller can
    /// persist it next to the alerts as a `trade.yaml` for reproducible
    /// rebuilds.
    pub spec: TradeSpec,
}

/// Entry order mode on the enter alert. `Stop` (the default) places a
/// pending stop order at the geometry anchor; `Market` fires a market
/// order at the next opportunity the worker sees the alert. `Market`
/// disables the entry-offset pips since there is no pending level to
/// offset from. `Limit` places a pending limit order at the geometry
/// anchor — it fills on a pullback *back* to the level (the Quasimodo
/// entry used by `--strategy-v2`), the mirror of `Stop`'s break-through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryMode {
    #[default]
    Stop,
    Market,
    Limit,
}

/// Declarative form of every answer the [`build_pattern`] questionnaire
/// collects. Drives both the interactive and `--from-file` paths so the
/// two cannot drift.
///
/// Optional fields apply the same defaults the interactive prompts do.
/// `tp_price` is required — there's no sensible default for an absolute
/// take-profit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeSpec {
    pub pattern: TradePattern,
    pub instrument: String,
    pub account: String,
    /// Broker the alerts target. Defaults to OANDA when omitted.
    #[serde(default = "default_broker")]
    pub broker: BrokerKind,
    /// Wall-clock end of the trade's validity window. Must be in the
    /// future relative to *now* at build time.
    pub trade_expiry: DateTime<Utc>,
    /// Risk per trade as a percent of equity. Mutually exclusive with
    /// `risk_amount` — when `risk_amount` is set this is ignored.
    #[serde(default = "default_risk_pct")]
    pub risk_pct: f64,
    /// Risk per trade as an absolute home-currency amount (e.g. 5.0 =
    /// 5 AUD risked on the stop-loss distance). When set, takes
    /// precedence over `risk_pct` and lands on `Intent::risk_amount`.
    #[serde(default)]
    pub risk_amount: Option<f64>,
    /// When true, the entry alert is built with `dry_run: true` so the
    /// worker logs the order but does not push to the broker. Vetos /
    /// preps are unaffected.
    #[serde(default)]
    pub dry_run: bool,
    /// Opt into multi-shot entry behaviour: the worker re-arms the
    /// entry up to this many attempts after stop-outs, until either
    /// the cap is reached or a veto/trade_expiry clears the setup.
    /// `0` (the default) preserves single-shot behaviour; any positive
    /// value opts into multi-shot. The non-Enter / missing-trade_id
    /// rules are enforced by `Intent::validate`.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub max_retries: u32,
    /// Optional bar-based expiry for the resting entry order (1..=5).
    /// When set, the worker cancels the pending stop/limit if it hasn't
    /// filled within N bars of placement — pulling a never-triggered
    /// breakout-stop rather than letting it rest until the entry
    /// deadline. Lands on the `05-enter` intent only. Requires the Pine
    /// study to ship the `next_candle_timestamp_1..5` plots (the worker
    /// indexes that menu with this N). Omit to keep today's behaviour
    /// (rest until the entry deadline). Out-of-range values are rejected
    /// by the worker at fire time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_bars: Option<u32>,
    /// Preps to omit from the bundle entirely. Each name listed here is
    /// dropped from both the emitted prep alerts *and* the entry's
    /// `requires_preps` gate. Use for setups where a step doesn't apply
    /// (e.g. skip "retest" on stocks that don't retest necklines, or
    /// skip both when arriving late to a setup that has already played
    /// out its preps). Unknown names are rejected.
    #[serde(default)]
    pub skip_preps: Vec<String>,
    /// Entry stop-trigger offset in pips from the geometry anchor.
    /// Omit to use the pattern's default (1 pip).
    #[serde(default)]
    pub entry_offset_pips: Option<f64>,
    /// Stop-loss offset in pips from the geometry anchor. Same default
    /// behaviour as `entry_offset_pips`.
    #[serde(default)]
    pub sl_offset_pips: Option<f64>,
    /// Override the pattern's default SL anchor. Omit to use the pattern
    /// default (`signal_high` for H&S, `signal_low` for iH&S — the latched
    /// pattern extreme, stable across a confirmation re-fire). Set to
    /// `recent_high` / `recent_low` to anchor against Pine's `recent_high` /
    /// `recent_low` shell fields instead, which span the indicator's
    /// `sl_lookback` window of bars *strictly preceding* the signal bar, or
    /// `high` / `low` to anchor to the triggering candle's own wick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sl_anchor: Option<PriceAnchor>,
    /// Take-profit absolute price. The worker treats this verbatim and
    /// does not consult the shell.
    pub tp_price: f64,
    /// Optional **absolute** stop-loss price. When set, the enter intent
    /// carries `PriceRef::Absolute` for the SL instead of anchoring to
    /// geometry + `sl_offset_pips` — used by the position-tool direct
    /// entry, where the operator drew the stop at a fixed price rather
    /// than relative to a pattern extreme. When `None` (the default,
    /// every pattern path), SL stays geometry-anchored and the yaml is
    /// byte-identical to pre-feature output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sl_price: Option<f64>,
    /// Entry window end as a percentage of (trade_expiry − now). Default
    /// 80 — leaves a tail of trade_expiry to chase a late retest only if
    /// the operator extends the window.
    #[serde(default = "default_entry_deadline_pct")]
    pub entry_deadline_pct: u32,
    /// Rhai script that gates entry placement. Lands on
    /// `Intent::allow_entry` as `Tunable::Script(...)`. The shell-side
    /// vocabulary (`signal_confirmed`, `pattern_range`, `tp_distance`,
    /// `r_multiple`, etc.) is documented in `core::rules`. Omit to let
    /// the worker fall through to the unconditional accept default.
    /// Static-bool isn't supported here because the only sensible value
    /// is a script — a literal `true` would just be redundant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_entry: Option<String>,
    /// Entry order mode. Default `Stop` preserves today's pending stop-
    /// entry behaviour (the spec yaml stays byte-identical when the
    /// field is absent). `Market` swaps to a market order — useful for
    /// confirmed-candle entries where waiting for a stop level would
    /// just add slippage.
    #[serde(default, skip_serializing_if = "is_default_entry_mode")]
    pub entry_mode: EntryMode,
    /// When true, the worker rejects the entry unless the incoming shell
    /// carries `golden: Some(true)`. AND-composed with [`Self::allow_entry`]
    /// — both gates must pass. Default `false` = no gate, byte-identical
    /// to pre-feature spec yaml.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_golden: bool,
    /// When true, the worker rejects the entry unless the incoming shell
    /// carries `confirmed: Some(true)`. Symmetric with [`Self::needs_golden`]
    /// and independent of it — the candle gate checks golden first, then
    /// confirmed, so setting both is a stricter "golden AND confirmed"
    /// entry. AND-composed with [`Self::allow_entry`]. Lands on the enter
    /// intent only. Default `false` = no gate, byte-identical to
    /// pre-feature spec yaml. (Distinct from [`Self::needs_confirmed_close`],
    /// which gates the close-on-reversal alert.)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_confirmed: bool,
    /// When true, mark the consolidated `06-close-on-reversal` alert
    /// as gated on an active news window for this `trade_id`. Adds
    /// `news` to the emitted intent's `inside_window` list. If
    /// [`Self::sr_reversal_ranges`] is also set the alert is OR-gated
    /// on news *or* price-band. Default `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub close_on_news: bool,
    /// When non-empty, mark the consolidated `06-close-on-reversal`
    /// alert as gated on the broker's current price sitting inside
    /// at least one of these `[lo, hi]` bands. Adds `price` to the
    /// emitted intent's `inside_window` and populates `sr_bands`.
    /// Bands are computed by the Python side from chart-drawn
    /// `support` / `resistance` single lines plus a width percentage.
    /// If [`Self::close_on_news`] is also true, the close is
    /// OR-gated. Default empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sr_reversal_ranges: Vec<[f64; 2]>,
    /// **Experimental, default OFF.** When true, the emitted
    /// `06-close-on-reversal` intent carries `veto_on_reversal: true`, so
    /// a reversal off one of the `sr_reversal_ranges` bands *also* writes
    /// a `reversal` veto blocking the upcoming `enter` (not just flattens
    /// an open position). Only takes effect when [`Self::sr_reversal_ranges`]
    /// is non-empty — there must be a price window for the reversal to
    /// fire off. Default `false` = byte-identical to pre-feature spec yaml.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub veto_on_reversal: bool,
    /// When true, the emitted `06-close-on-reversal` intent carries
    /// `needs_confirmed: true` instead of the default
    /// `needs_golden: true`. Lets the operator opt the close trigger
    /// down from "golden-reversal candle" to "confirmed candle that
    /// is not necessarily golden". Mutually exclusive with the
    /// default golden path; the resulting intent has exactly one of
    /// the two flags set. Default `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_confirmed_close: bool,
    /// Prep steps that have a `<prep>-expiry` cutoff line on the chart.
    /// Each name here gets an extra `08-prep-expire-<step>` alert: a
    /// drawing-bound `prep-expire` that, when its vertical line is
    /// crossed, blocks any further `prep` for that step — so a setup
    /// whose prep lands too late never enters. Names must be in
    /// [`KNOWN_PREP_NAMES`] and must not also be in [`Self::skip_preps`]
    /// (you can't expire a prep you've dropped). Default empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prep_expiries: Vec<String>,
    /// M / W (double-top / double-bottom) static geometry, baked at arm
    /// time. Present only for `TradePattern::{M,W}` — its presence is
    /// what routes [`build_trade_from_spec`] through [`build_mw_pattern`]
    /// instead of the H&S questionnaire path. The worker recomputes
    /// entry/SL/TP from these anchors + the live shell OHLC, so the
    /// enter alert carries no fixed entry/stop_loss/take_profit. See
    /// [`MwSpec`] and `core::intent::MwParams`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mw: Option<MwSpec>,
    /// Instrument pip size, resolved from `instrument-lookup` at arm time
    /// (`asset.pip_size`) or the `--pip-size` override. Baked onto the
    /// enter intent's top-level `pip_size` so the worker scales every
    /// `offset_pips` correctly instead of relying on its forex-default
    /// fallback. Set for both H&S and M/W; for M/W it matches
    /// [`MwSpec::pip_size`]. Absent = the worker falls back to its secret /
    /// default (pre-feature behaviour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pip_size: Option<f64>,
    /// What the market-hours blackout sweep should do with this trade's
    /// still-pending resting order if it's caught inside the instrument's
    /// daily close→open gap. Lands on the `05-enter` intent's
    /// `blackout_close`. Default [`BlackoutCloseAction::CancelResting`]
    /// (the incident fix: cancel the unfilled order, never close a filled
    /// position); set `cancel-and-close` to also flatten an open position
    /// on the instrument. Byte-identical to pre-feature spec yaml when left
    /// at the default.
    #[serde(default, skip_serializing_if = "is_default_blackout_close")]
    pub blackout_close: BlackoutCloseAction,
    /// Continuous at-entry level vetos (Bug #12) baked onto the `05-enter`
    /// intent. For H&S these are the pcl-exhausted (`too-low`) and
    /// invalidation (`too-high`) price levels, computed at arm time from the
    /// fib + invalidation drawings; `build_enter_alert` copies them onto the
    /// enter intent so the worker rejects an entry already past the level
    /// even when no cross-event guard fired. Empty for M/W and for
    /// pre-feature specs. `#[serde(default)]` keeps spec yaml byte-identical
    /// when unset.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_level_vetos: Vec<trade_control_core::intent::EntryLevelVeto>,
    /// How to recover an H&S / iH&S stop entry that goes wrong-side at
    /// resolve time (price broke the trigger during the
    /// signal-confirmation wait). Lands on the `05-enter` intent's
    /// `EntrySpec::Stop::recover_entry` (only for `EntryMode::Stop`).
    /// `Skip` (default) emits no `recover_entry` field → today's drop
    /// behaviour; `Market`/`Limit` opt in. M/W never reaches this builder.
    /// `#[serde(default)]` keeps spec yaml byte-identical when unset.
    #[serde(default, skip_serializing_if = "is_default_recover_entry")]
    pub recover_entry: RecoverEntryAction,
    /// **strategy-v2 (H&S only), default OFF.** Arm a *second* enter — the
    /// Quasimodo limit — alongside the normal stop entry, on the same setup.
    /// The QM enter carries no preps (break-and-close / retest skipped), is
    /// gated only on a confirmed signal candle, and rests as a limit order at
    /// the same signal level the stop entry anchors to. Whichever of the two
    /// fires first wins: the worker's retry gate cancels the other's resting
    /// order (both enters share this `trade_id` + a non-zero `max_retries`),
    /// and an already-filled position blocks the sibling. The stop enter is
    /// emitted first so it wins a same-bar tie. Off = byte-identical
    /// single-enter spec yaml.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strategy_v2: bool,
    /// Break-even stop management (BUG-replay-no-breakeven-stop-at-50pct).
    /// **Default ON at 50%.** Bakes a [`Breakeven`] rule onto the `05-enter`
    /// intent (H&S, M/W, and the strategy-v2 QM enter) so the live worker's
    /// position cron moves the stop-loss to break-even (the entry price) once a
    /// candle closes past this fraction of the entry→TP distance.
    ///
    /// `Some(f)` arms at fraction `f` (0.5 = halfway); the default function
    /// supplies `Some(0.5)`. Set `breakeven_pct: null` in the spec yaml to
    /// **disable** break-even for this trade. Out-of-`(0,1)` values are clamped
    /// to 0.5 by the worker/replay (`Breakeven::sane`).
    #[serde(default = "default_breakeven_pct")]
    pub breakeven_pct: Option<f64>,
}

/// Default for [`TradeSpec::breakeven_pct`]: break-even is **on at 50%** unless
/// the operator explicitly sets `breakeven_pct: null`. The standing lesson
/// ("once profit reaches 50%, set SL to break-even") is the default, not an
/// opt-in.
fn default_breakeven_pct() -> Option<f64> {
    Some(trade_control_core::intent::DEFAULT_BREAKEVEN_THRESHOLD)
}

/// Build the [`Breakeven`] rule for an enter from a spec's `breakeven_pct`:
/// `Some(f)` → armed at `f`; `None` → no break-even (operator opted out).
fn breakeven_from_pct(breakeven_pct: Option<f64>) -> Option<trade_control_core::intent::Breakeven> {
    breakeven_pct.map(|threshold| trade_control_core::intent::Breakeven { threshold })
}

/// Skip-serializing predicate for [`TradeSpec::blackout_close`] — keeps a
/// default spec yaml byte-identical to pre-feature specs.
fn is_default_blackout_close(a: &BlackoutCloseAction) -> bool {
    matches!(a, BlackoutCloseAction::CancelResting)
}

/// Skip-serializing predicate for [`TradeSpec::recover_entry`] — `Skip`
/// is the default (emit no `recover_entry`), keeping spec yaml
/// byte-identical to pre-feature specs.
fn is_default_recover_entry(a: &RecoverEntryAction) -> bool {
    matches!(a, RecoverEntryAction::Skip)
}

/// CLI-side mirror of [`trade_control_core::intent::MwParams`]. Kept as a
/// distinct type so the spec yaml is decoupled from the wire intent — the
/// builder copies it onto the enter intent's `mw` field. All prices are
/// **MID** prices read off the chart; the worker does the mid→bid/ask
/// correction at resolution time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MwSpec {
    /// `C` — the neckline (entry trigger anchor + abort level).
    pub neckline: f64,
    /// `B` — the first peak (M) / first trough (W); SL anchor base.
    pub first_point: f64,
    /// `A` — the runup start. Audit / log only; fed the arm-time
    /// neckline-% gate, not the worker's entry geometry.
    pub runup_start: f64,
    /// `D` — the optional drawn right shoulder (4-point path). When set the
    /// worker arms immediately. `#[serde(default)]` keeps a 3-point spec
    /// yaml byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right_shoulder: Option<f64>,
    /// Broker spread in pips, read at arm time. `>= 0`.
    pub spread_pips: f64,
    /// Instrument pip size at arm time (e.g. `0.0001`). `> 0`.
    pub pip_size: f64,
}

impl MwSpec {
    /// Lower into the signed wire form the worker resolves.
    fn to_params(self) -> trade_control_core::intent::MwParams {
        trade_control_core::intent::MwParams {
            neckline: self.neckline,
            first_point: self.first_point,
            runup_start: self.runup_start,
            right_shoulder: self.right_shoulder,
            spread_pips: self.spread_pips,
            pip_size: self.pip_size,
        }
    }
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn is_default_entry_mode(m: &EntryMode) -> bool {
    *m == EntryMode::default()
}

fn default_broker() -> BrokerKind {
    BrokerKind::Oanda
}

fn default_risk_pct() -> f64 {
    DEFAULT_RISK_PCT
}

fn default_entry_deadline_pct() -> u32 {
    DEFAULT_ENTRY_DEADLINE_PCT
}

/// Read a `trade.yaml` file and return its [`TradeSpec`]. Pure I/O +
/// deser — validation lives in [`build_trade_from_spec`].
pub fn load_spec_from_file(path: &Path) -> Result<TradeSpec> {
    let text =
        fs::read_to_string(path).with_context(|| format!("reading spec {}", path.display()))?;
    let spec: TradeSpec = serde_yaml::from_str(&text)
        .with_context(|| format!("parsing spec {} as YAML", path.display()))?;
    Ok(spec)
}

/// Run the full questionnaire for a pattern and return the assembled
/// trade. Output dir is not touched here — see [`write_trade`].
pub fn build_trade_interactive(pattern: TradePattern, now: DateTime<Utc>) -> Result<BuiltTrade> {
    match pattern {
        TradePattern::Hs | TradePattern::Ihs => {
            build_pattern(pattern, PatternGeometry::for_pattern(pattern), now)
        }
        // M / W have no chart-side geometry to prompt for — the path
        // anchors come from `tv-arm` reading the drawing, never an
        // interactive questionnaire. There's nothing sensible to ask the
        // operator here, so the only supported path is `--from-file` (a
        // spec `tv-arm` writes, carrying `mw`). Reject the interactive
        // entry explicitly rather than prompt for anchors by hand.
        TradePattern::M | TradePattern::W => Err(eyre!(
            "pattern {} is built from a chart by `tv-arm`, not interactively — \
             run `tv-arm` (or pass a prebuilt spec with `--from-file`)",
            pattern.label()
        )),
    }
}

/// How strictly to validate a [`TradeSpec`] that's about to be built.
///
/// The on-disk signing path (`build-trade --from-file`, `tv-arm
/// --register-plan`) feeds the live worker, so a stale `trade_expiry`
/// would arm a plan that can never enter — that's a hard error. The
/// offline `tv-arm --plan-out` path (no worker POST) is used for
/// replay / inspection of historical setups, where an expired window
/// or an in-window news event is expected; there we only warn so the
/// JSON still gets written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildStrictness {
    /// Bound for the live worker — expired / news-overlapping setups are
    /// rejected.
    Strict,
    /// Offline `--plan-out` only — those conditions warn instead of erroring.
    Lenient,
}

/// Build a trade from a pre-filled [`TradeSpec`] with no prompts. Used by
/// the `--from-file` flag on `build-trade`. Validates the spec against
/// the same rules the prompts would enforce, then assembles the alerts.
///
/// `strictness` controls whether time-sensitive checks (`trade_expiry` in
/// the past, news in the window) are hard errors ([`BuildStrictness::Strict`],
/// the live path) or warnings ([`BuildStrictness::Lenient`], offline
/// `--plan-out`).
pub fn build_trade_from_spec(
    mut spec: TradeSpec,
    now: DateTime<Utc>,
    strictness: BuildStrictness,
) -> Result<BuiltTrade> {
    let is_mw = matches!(spec.pattern, TradePattern::M | TradePattern::W);
    // `mw` and the pattern must agree: M/W require the baked path
    // geometry, H&S/IH&S must not carry it. Catch a hand-edited spec
    // that mixes them before any alert is built.
    match (is_mw, spec.mw.is_some()) {
        (true, false) => {
            return Err(eyre!(
                "pattern {} requires `mw` path geometry in the spec (neckline, first_point, \
                 runup_start, spread_pips, pip_size) — it's normally written by `tv-arm`",
                spec.pattern.label()
            ));
        }
        (false, true) => {
            return Err(eyre!(
                "pattern {} must not carry `mw` geometry — that's only for M / W setups",
                spec.pattern.label()
            ));
        }
        _ => {}
    }
    if spec.instrument.trim().is_empty() {
        return Err(eyre!("instrument is required"));
    }
    if let Some(canonical) =
        validate_instrument(spec.broker, Some(&spec.account), &spec.instrument)?
    {
        // The cache redirected (e.g. "XAG/USD" → "Spot Silver"). Swap the
        // spec to the canonical name so every downstream alert, manifest,
        // and persisted trade.yaml uses the spelling TN's API accepts.
        tracing::warn!(
            input = %spec.instrument,
            canonical = %canonical,
            "instrument resolved to canonical broker name",
        );
        spec.instrument = canonical;
    }
    if spec.account.trim().is_empty() {
        return Err(eyre!("account is required"));
    }
    if spec.trade_expiry <= now {
        match strictness {
            BuildStrictness::Strict => {
                return Err(eyre!("trade_expiry must be in the future"));
            }
            BuildStrictness::Lenient => {
                tracing::warn!(
                    trade_expiry = %spec.trade_expiry.to_rfc3339(),
                    now = %now.to_rfc3339(),
                    "trade_expiry is in the past — allowed because this is an offline \
                     --plan-out build (would be rejected on the live worker path)",
                );
            }
        }
    }
    if !spec.tp_price.is_finite() {
        return Err(eyre!("tp_price must be a finite number"));
    }
    if spec.entry_deadline_pct == 0 || spec.entry_deadline_pct > 100 {
        return Err(eyre!("entry_deadline_pct must be in 1..=100"));
    }
    match spec.risk_amount {
        Some(amount) => {
            if !amount.is_finite() || amount <= 0.0 {
                return Err(eyre!("risk_amount must be a positive finite number"));
            }
        }
        None => {
            if !spec.risk_pct.is_finite() || spec.risk_pct <= 0.0 {
                return Err(eyre!("risk_pct must be a positive finite number"));
            }
        }
    }
    for name in &spec.skip_preps {
        if !KNOWN_PREP_NAMES.contains(&name.as_str()) {
            return Err(eyre!(
                "skip_preps name {name:?} is not a known prep; expected one of {KNOWN_PREP_NAMES:?}"
            ));
        }
    }
    for name in &spec.prep_expiries {
        if !KNOWN_PREP_NAMES.contains(&name.as_str()) {
            return Err(eyre!(
                "prep_expiries name {name:?} is not a known prep; expected one of \
                 {KNOWN_PREP_NAMES:?}"
            ));
        }
        if spec.skip_preps.iter().any(|s| s == name) {
            return Err(eyre!(
                "prep_expiries name {name:?} is also in skip_preps — can't expire a prep \
                 that's been dropped"
            ));
        }
    }
    // M / W are structurally different from H&S — no prep chain, no
    // drawing-bound vetos, worker-computed entry/SL/TP. They get their
    // own builder rather than being forced through `PatternGeometry`.
    if is_mw {
        return build_mw_pattern(spec, now);
    }
    let mut geometry = PatternGeometry::for_pattern(spec.pattern);
    if let Some(override_anchor) = spec.sl_anchor {
        // Validate the override makes directional sense — a short can't
        // have its SL anchored to a low, and vice versa. (We don't gate
        // close-anchored here because that's already an odd choice the
        // operator might want for ATR-style stops.)
        let ok = matches!(
            (geometry.direction, override_anchor),
            (
                Direction::Short,
                PriceAnchor::High | PriceAnchor::RecentHigh | PriceAnchor::SignalHigh
            ) | (
                Direction::Long,
                PriceAnchor::Low | PriceAnchor::RecentLow | PriceAnchor::SignalLow
            ) | (_, PriceAnchor::Close)
        );
        if !ok {
            return Err(eyre!(
                "sl_anchor {:?} is incompatible with {} direction",
                override_anchor,
                match geometry.direction {
                    Direction::Long => "long",
                    Direction::Short => "short",
                }
            ));
        }
        geometry.sl_anchor = override_anchor;
    }
    let entry_offset_pips = spec
        .entry_offset_pips
        .unwrap_or(geometry.entry_offset_default);
    let sl_offset_pips = spec.sl_offset_pips.unwrap_or(geometry.sl_offset_default);
    assemble_trade(spec, geometry, entry_offset_pips, sl_offset_pips, now)
}

/// Preps the H&S / IH&S pipeline can emit. Used to validate
/// `skip_preps` so a typo doesn't silently leave a requirement in
/// place.
const KNOWN_PREP_NAMES: &[&str] = &["break-and-close", "retest"];

/// Persist a built trade: each alert as `<basename>.yaml` (signed,
/// TradingView shell placeholders), plus a `manifest.yaml` summarising
/// the set. Returns the resolved output directory so the caller can
/// print it.
pub fn write_trade(trade: &BuiltTrade, key: &[u8; KEY_LEN], out_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    for alert in &trade.alerts {
        // Only Pine-bound alerts (the entry, and the optional
        // close-on-reversal which fires from the *opposite* direction
        // of the same `Candle Signals` study) can resolve
        // `{{plot("…")}}` placeholders. Vetos and preps fire from
        // chart drawings — TV delivers any `{{plot(…)}}` literally
        // there, which crashes the worker's YAML parser. Strip those
        // for drawings.
        let is_pine_bound =
            alert.basename == "05-enter" || alert.basename == "06-close-on-reversal";
        let body = if is_pine_bound {
            wrap_signed_template(&alert.intent, key)
        } else {
            wrap_signed_template_drawing(&alert.intent, key)
        }
        .map_err(|e| eyre!("sign {}: {e}", alert.basename))?;
        let path = out_dir.join(format!("{}.yaml", alert.basename));
        fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    }
    let manifest = render_manifest(trade);
    let manifest_path = out_dir.join("manifest.yaml");
    fs::write(&manifest_path, manifest)
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    // Persist the spec so a future run can rebuild this trade with
    // `--from-file trade.yaml` — useful for tweaking one field without
    // re-running the questionnaire from scratch.
    let spec_yaml = serde_yaml::to_string(&trade.spec).context("serialising trade.yaml")?;
    let spec_path = out_dir.join("trade.yaml");
    fs::write(&spec_path, spec_yaml).with_context(|| format!("writing {}", spec_path.display()))?;
    Ok(out_dir.to_path_buf())
}

fn render_manifest(trade: &BuiltTrade) -> String {
    let mut out = String::new();
    out.push_str(&format!("trade_id: {}\n", trade.trade_id));
    out.push_str(&format!("instrument: {}\n", trade.instrument));
    out.push_str(&format!(
        "trade_expiry: \"{}\"\n",
        trade.trade_expiry.to_rfc3339()
    ));
    out.push_str("alerts:\n");
    for alert in &trade.alerts {
        out.push_str(&format!("  - file: {}.yaml\n", alert.basename));
        out.push_str(&format!("    purpose: {}\n", alert.purpose));
        out.push_str(&format!("    action: {:?}\n", alert.intent.action));
        if let Some(name) = &alert.intent.name {
            out.push_str(&format!("    name: {name}\n"));
        }
        if let Some(step) = &alert.intent.step {
            out.push_str(&format!("    step: {step}\n"));
        }
        if let Some(level) = &alert.intent.level {
            out.push_str(&format!("    level: {level:?}\n"));
        }
        out.push_str(&format!(
            "    not_after: \"{}\"\n",
            alert.intent.not_after.to_rfc3339()
        ));
    }
    out
}

// ===== Shared questionnaire =====

fn build_pattern(
    pattern: TradePattern,
    geometry: PatternGeometry,
    now: DateTime<Utc>,
) -> Result<BuiltTrade> {
    let theme = ColorfulTheme::default();

    let instrument = prompt_instrument(&theme)?;
    let account: String = Input::with_theme(&theme)
        .with_prompt("account name (worker account index)")
        .interact_text()
        .map_err(|e| eyre!("account prompt: {e}"))?;
    let broker = prompt_broker(&theme)?;
    let trade_expiry = prompt_trade_expiry(&theme, &instrument, now)?;
    expiry::save(&instrument, trade_expiry)?;

    let risk_pct: f64 = Input::with_theme(&theme)
        .with_prompt("risk percent of equity")
        .default(DEFAULT_RISK_PCT)
        .interact_text()
        .map_err(|e| eyre!("risk_pct prompt: {e}"))?;

    let entry_offset_pips: f64 = Input::with_theme(&theme)
        .with_prompt(format!(
            "entry stop trigger offset (pips from {})",
            anchor_label(geometry.entry_anchor)
        ))
        .default(geometry.entry_offset_default)
        .interact_text()
        .map_err(|e| eyre!("entry offset prompt: {e}"))?;

    let sl_offset_pips: f64 = Input::with_theme(&theme)
        .with_prompt(format!(
            "stop-loss offset (pips from {})",
            anchor_label(geometry.sl_anchor)
        ))
        .default(geometry.sl_offset_default)
        .interact_text()
        .map_err(|e| eyre!("sl prompt: {e}"))?;

    let tp_price: f64 = Input::with_theme(&theme)
        .with_prompt("take-profit absolute price")
        .interact_text()
        .map_err(|e| eyre!("tp prompt: {e}"))?;

    let entry_deadline_pct: u32 = Input::with_theme(&theme)
        .with_prompt("entry window ends at (% of time to trade_expiry)")
        .default(DEFAULT_ENTRY_DEADLINE_PCT)
        .interact_text()
        .map_err(|e| eyre!("entry deadline prompt: {e}"))?;

    let spec = TradeSpec {
        pattern,
        instrument,
        account,
        broker,
        trade_expiry,
        risk_pct,
        risk_amount: None,
        dry_run: false,
        max_retries: 0,
        expiry_bars: None,
        needs_golden: false,
        needs_confirmed: false,
        close_on_news: false,
        sr_reversal_ranges: Vec::new(),
        veto_on_reversal: false,
        needs_confirmed_close: false,
        prep_expiries: Vec::new(),
        skip_preps: Vec::new(),
        entry_offset_pips: Some(entry_offset_pips),
        sl_offset_pips: Some(sl_offset_pips),
        tp_price,
        // Interactive/questionnaire path always anchors SL to geometry.
        sl_price: None,
        entry_deadline_pct,
        allow_entry: None,
        entry_mode: EntryMode::default(),
        sl_anchor: None,
        mw: None,
        pip_size: None,
        // Interactive path keeps the safe default (cancel a resting order,
        // never close a position). The `--blackout-close` flag lives on the
        // `--from-file` / scripted path.
        blackout_close: BlackoutCloseAction::default(),
        entry_level_vetos: Vec::new(),
        // Interactive path keeps today's drop-on-wrong-side behaviour. The
        // `--recover-entry` opt-in lives on the scripted / tv-arm path.
        recover_entry: RecoverEntryAction::Skip,
        // strategy-v2 (dual stop + QM enter) is a tv-arm opt-in, not an
        // interactive-questionnaire option.
        strategy_v2: false,
        // Break-even on at 50% by default (the standing lesson).
        breakeven_pct: default_breakeven_pct(),
    };
    assemble_trade(spec, geometry, entry_offset_pips, sl_offset_pips, now)
}

/// Common alert-assembly path shared by the interactive and
/// `--from-file` modes. Both already-resolved offset values are passed
/// in so callers can apply pattern defaults exactly once.
fn assemble_trade(
    spec: TradeSpec,
    geometry: PatternGeometry,
    entry_offset_pips: f64,
    sl_offset_pips: f64,
    now: DateTime<Utc>,
) -> Result<BuiltTrade> {
    let trade_id = mint_trade_id(spec.pattern, &spec.instrument)?;
    let entry_deadline = derive_entry_deadline(now, spec.trade_expiry, spec.entry_deadline_pct);
    let veto_expiry = spec.trade_expiry + DEFAULT_POST_EXPIRY_GRACE;

    let skip_bnc = spec.skip_preps.iter().any(|n| n == "break-and-close");
    let skip_retest = spec.skip_preps.iter().any(|n| n == "retest");
    let mut alerts = vec![
        // Invalidation veto: price ran back past the right shoulder →
        // structure broken, thesis dead. ClosePositions: flatten any
        // open trade.
        build_invalidation_alert(
            &spec.instrument,
            &trade_id,
            geometry.invalidation_veto_name,
            VetoLevel::ClosePositions,
            veto_expiry,
            &spec.broker,
            &spec.account,
            now,
        ),
        // Pcl-exhausted veto: price ran most of the way to TP without us
        // in. StopNextEntry only — it blocks a *late* entry and must
        // never close (or cancel) an open position. A breach in the
        // trade's favour is profit, not a reason to exit. The chart side
        // binds this one to a computed price (pcl-exhausted) rather than
        // a drawing. See BUG-too-low-closes-positions.md (demo trade 046).
        build_invalidation_alert(
            &spec.instrument,
            &trade_id,
            geometry.pcl_exhausted_veto_name,
            VetoLevel::StopNextEntry,
            veto_expiry,
            &spec.broker,
            &spec.account,
            now,
        ),
        build_trade_expiry_alert(
            &spec.instrument,
            &trade_id,
            spec.trade_expiry,
            veto_expiry,
            &spec.broker,
            &spec.account,
            now,
        ),
    ];
    if !skip_bnc {
        alerts.push(build_break_and_close_alert(
            &spec.instrument,
            &trade_id,
            spec.trade_expiry,
            &spec.broker,
            &spec.account,
            now,
        ));
    }
    if !skip_retest {
        alerts.push(build_retest_alert(
            &spec.instrument,
            &trade_id,
            spec.trade_expiry,
            &spec.broker,
            &spec.account,
            now,
        ));
    }
    // One prep-expire alert per chart-drawn `<prep>-expiry` line. When
    // its vertical line is crossed, the worker blocks further preps for
    // that step, so a setup whose prep lands too late never enters.
    for step in &spec.prep_expiries {
        alerts.push(build_prep_expire_alert(
            &spec.instrument,
            &trade_id,
            step,
            veto_expiry,
            &spec.broker,
            &spec.account,
            now,
        ));
    }
    alerts.push(build_enter_alert(
        &spec.instrument,
        &trade_id,
        &geometry,
        entry_deadline,
        entry_offset_pips,
        sl_offset_pips,
        spec.tp_price,
        spec.sl_price,
        spec.risk_pct,
        spec.risk_amount,
        spec.dry_run,
        spec.max_retries,
        spec.expiry_bars,
        spec.allow_entry.as_deref(),
        spec.entry_mode,
        spec.needs_golden,
        spec.needs_confirmed,
        &spec.skip_preps,
        spec.pip_size,
        spec.blackout_close,
        &spec.broker,
        &spec.account,
        // The enter must check the `reversal` veto only when a
        // reversal-close that writes it actually exists for this setup —
        // i.e. the flag is armed AND there are sr bands to reverse off.
        spec.veto_on_reversal && !spec.sr_reversal_ranges.is_empty(),
        &spec.entry_level_vetos,
        spec.recover_entry,
        false, // BCR leg → 05-enter
        breakeven_from_pct(spec.breakeven_pct),
    ));
    // strategy-v2: a second enter — the Quasimodo entry — armed alongside the
    // BCR stop entry on the same setup. No preps (both skipped),
    // confirmed-candle gated, `09-enter-qm` basename. It shares this trade_id +
    // max_retries so the worker retry gate treats the two enters as attempts of
    // one trade and cancels the loser's resting order. Pushed *after* the BCR
    // enter so the BCR wins a same-bar tie. The at-entry level vetos
    // (too-high/too-low) still apply — they're invalidation levels independent
    // of order type.
    //
    // The QM leg's entry spec is built *identical* to standalone --quasimodo's
    // 05-enter: `EntryMode::Stop` at signal_low − 1 pip (`entry_offset_pips`,
    // the same value the BCR leg uses) with `RecoverEntryAction::Limit` as the
    // wrong-side fallback. The earlier `EntryMode::Limit` / offset 0 form built
    // a bare sell-limit resting *at* the level; for a short where price has
    // already pushed below the level that limit is on the wrong side of market,
    // the engine rejects it as geometry-inconsistent, and with no recover_entry
    // the whole leg evaporated (demo trade 031, CAD/JPY). A resting stop below
    // market is valid for a short and fills on the pullback, exactly as
    // standalone --quasimodo does.
    if spec.strategy_v2 {
        let qm_skip_preps = vec!["break-and-close".to_string(), "retest".to_string()];
        alerts.push(build_enter_alert(
            &spec.instrument,
            &trade_id,
            &geometry,
            entry_deadline,
            entry_offset_pips,
            sl_offset_pips,
            spec.tp_price,
            spec.sl_price,
            spec.risk_pct,
            spec.risk_amount,
            spec.dry_run,
            spec.max_retries,
            spec.expiry_bars,
            spec.allow_entry.as_deref(),
            EntryMode::Stop, // identical order shape to standalone --quasimodo
            spec.needs_golden,
            true, // QM is always confirmed-candle gated
            &qm_skip_preps,
            spec.pip_size,
            spec.blackout_close,
            &spec.broker,
            &spec.account,
            spec.veto_on_reversal && !spec.sr_reversal_ranges.is_empty(),
            &spec.entry_level_vetos,
            RecoverEntryAction::Limit, // wrong-side fallback, like standalone QM
            true,                      // QM leg → 09-enter-qm
            breakeven_from_pct(spec.breakeven_pct),
        ));
    }
    if spec.close_on_news || !spec.sr_reversal_ranges.is_empty() {
        alerts.push(build_close_on_reversal_alert(
            &spec.instrument,
            &trade_id,
            spec.trade_expiry,
            &spec.broker,
            &spec.account,
            spec.close_on_news,
            spec.sr_reversal_ranges.clone(),
            spec.needs_confirmed_close,
            spec.veto_on_reversal,
        ));
    }

    // Sign-time script validation. Catches typos / wrong-return-type /
    // unknown-variable refs in any `Tunable::Script` field (today just
    // `allow_entry`) before the alerts are signed. Errors from every
    // alert are aggregated so the operator sees the full punch list,
    // not one-at-a-time.
    let script_errors: Vec<String> = alerts
        .iter()
        .flat_map(|alert| {
            crate::script_validator::validate(&alert.intent)
                .into_iter()
                .map(move |e| format!("{}: {e}", alert.basename))
        })
        .collect();
    if !script_errors.is_empty() {
        return Err(eyre!(
            "sign-time script validation failed:\n  - {}",
            script_errors.join("\n  - ")
        ));
    }

    Ok(BuiltTrade {
        trade_id,
        instrument: spec.instrument.clone(),
        trade_expiry: spec.trade_expiry,
        alerts,
        spec,
    })
}

// ===== M / W (double-top / double-bottom) assembly =====

/// Assemble an M / W trade bundle. Structurally distinct from H&S: no
/// prep chain, no drawing-bound invalidation vetos, and the worker — not
/// the chart — computes entry/SL/TP from the baked `mw` anchors plus the
/// live shell OHLC (see `core::intent::mw_resolution`).
///
/// The bundle is exactly five alerts:
///
/// 1. `01-veto-mw-cancel` — `CancelPending`. Fires intra-bar when price
///    crosses the 1.3-extension of the neckline→first-point leg; cancels
///    the pending stop and disarms. Also enforces the two-peaks
///    alignment ceiling implicitly.
/// 2. `01-veto-mw-abort` — `CancelPending`. Fires when a candle *closes*
///    back through the neckline; the breakout failed. `CancelPending`
///    not `ClosePositions` — once filled the trade rides its own SL/TP.
/// 3. `01-veto-mw-overshoot` — `CancelPending`. Fires intra-bar when price
///    runs 180% of the top→neckline leg (the move is essentially complete;
///    a late entry's R:R no longer justifies opening). Static arm-time
///    level — over-vetoes (the safe direction) as the pattern grows.
/// 4. `02-veto-trade-expiry` — unchanged from H&S (time-fired
///    `ClosePositions` at wall-clock expiry).
/// 5. `05-enter` — per-bar stop entry carrying the baked `mw` params and
///    no fixed entry/stop_loss/take_profit; gated only by the four
///    vetos above (no preps), `max_retries: 0`.
fn build_mw_pattern(spec: TradeSpec, now: DateTime<Utc>) -> Result<BuiltTrade> {
    // `is_mw` already guaranteed `spec.mw` is Some at the dispatch site;
    // ? here is belt-and-braces and keeps this fn self-contained.
    let mw = spec
        .mw
        .ok_or_else(|| eyre!("build_mw_pattern called without mw geometry"))?;
    let direction = match spec.pattern {
        TradePattern::M => Direction::Short,
        TradePattern::W => Direction::Long,
        other => {
            return Err(eyre!(
                "build_mw_pattern called for non-M/W pattern {other:?}"
            ));
        }
    };

    // Build-time SL-vs-spread floor (hard limit; the worker re-checks at fire
    // time against the live spread). The M/W entry/SL are pure functions of the
    // baked anchors + arm-time spread, so we can reject a too-tight stop before
    // signing — covers both tv-arm (which routes through here) and hand-crafted
    // `build-trade --from-file` specs. Same constant + decision as the worker:
    // `sl_spread_floor_violation`. (H&S has no build-time SL — it anchors to the
    // fire-time signal extreme — so H&S relies on the worker gate alone.)
    {
        let params = mw.to_params();
        let spread_price = params.spread_pips * params.pip_size;
        let (entry, sl, _tp) = trade_control_core::intent::mw_static_prices(direction, &params);
        let sl_distance = (entry - sl).abs();
        if trade_control_core::intent::sl_spread_floor_violation(sl_distance, spread_price) {
            let min_sl = trade_control_core::intent::SL_MIN_SPREAD_MULTIPLE * spread_price;
            return Err(eyre!(
                "M/W stop-loss is too close to the spread: SL distance {sl_distance} < required \
                 {min_sl} ({}× the {spread_price} spread). Tighten the spread (arm in a better \
                 session) or widen the pattern.",
                trade_control_core::intent::SL_MIN_SPREAD_MULTIPLE
            ));
        }
    }

    let trade_id = mint_trade_id(spec.pattern, &spec.instrument)?;
    let entry_deadline = derive_entry_deadline(now, spec.trade_expiry, spec.entry_deadline_pct);
    let veto_expiry = spec.trade_expiry + DEFAULT_POST_EXPIRY_GRACE;

    let alerts = vec![
        build_mw_cancel_alert(
            &spec.instrument,
            &trade_id,
            veto_expiry,
            &spec.broker,
            &spec.account,
            now,
        ),
        build_mw_abort_alert(
            &spec.instrument,
            &trade_id,
            veto_expiry,
            &spec.broker,
            &spec.account,
            now,
        ),
        build_mw_overshoot_alert(
            &spec.instrument,
            &trade_id,
            veto_expiry,
            &spec.broker,
            &spec.account,
            now,
        ),
        build_trade_expiry_alert(
            &spec.instrument,
            &trade_id,
            spec.trade_expiry,
            veto_expiry,
            &spec.broker,
            &spec.account,
            now,
        ),
        build_mw_enter_alert(
            &spec.instrument,
            &trade_id,
            direction,
            mw,
            entry_deadline,
            spec.risk_pct,
            spec.risk_amount,
            spec.dry_run,
            spec.allow_entry.as_deref(),
            spec.needs_golden,
            spec.needs_confirmed,
            spec.blackout_close,
            &spec.broker,
            &spec.account,
            breakeven_from_pct(spec.breakeven_pct),
        ),
    ];

    // Same sign-time script validation as the H&S path: catch a bad
    // `allow_entry` script before signing.
    let script_errors: Vec<String> = alerts
        .iter()
        .flat_map(|alert| {
            crate::script_validator::validate(&alert.intent)
                .into_iter()
                .map(move |e| format!("{}: {e}", alert.basename))
        })
        .collect();
    if !script_errors.is_empty() {
        return Err(eyre!(
            "sign-time script validation failed:\n  - {}",
            script_errors.join("\n  - ")
        ));
    }

    Ok(BuiltTrade {
        trade_id,
        instrument: spec.instrument.clone(),
        trade_expiry: spec.trade_expiry,
        alerts,
        spec,
    })
}

/// `01-veto-mw-cancel` — `CancelPending`. The chart side binds this to a
/// computed price (the 1.3-extension cancel level) firing intra-bar. The
/// price + frequency are set on the chart in `tv-arm`'s `alert_spec`;
/// here we only mint the intent shape.
fn build_mw_cancel_alert(
    instrument: &str,
    trade_id: &str,
    veto_expiry: DateTime<Utc>,
    broker: &BrokerKind,
    account: &str,
    now: DateTime<Utc>,
) -> BuiltAlert {
    let id = format!("{trade_id}-mw-cancel");
    let mut intent = skeleton(
        Action::Veto,
        instrument,
        id,
        veto_expiry,
        *broker,
        account,
        trade_id,
    );
    intent.name = Some(MW_CANCEL_VETO_NAME.into());
    intent.ttl_hours =
        trade_control_core::tunable::Tunable::Static(ttl_hours_until(now, veto_expiry));
    intent.level = Some(VetoLevel::CancelPending);
    BuiltAlert {
        basename: AlertBasename::VetoMwCancel.as_str().into_owned(),
        purpose: "veto: mw-cancel (cancel pending if price crosses the 1.3 extension)".into(),
        intent,
    }
}

/// `01-veto-mw-abort` — `CancelPending`. Fires when a candle closes back
/// through the neckline (breakout failed). `CancelPending` not
/// `ClosePositions`: abort only matters while the entry is pending; a
/// filled trade rides its own SL/TP.
fn build_mw_abort_alert(
    instrument: &str,
    trade_id: &str,
    veto_expiry: DateTime<Utc>,
    broker: &BrokerKind,
    account: &str,
    now: DateTime<Utc>,
) -> BuiltAlert {
    let id = format!("{trade_id}-mw-abort");
    let mut intent = skeleton(
        Action::Veto,
        instrument,
        id,
        veto_expiry,
        *broker,
        account,
        trade_id,
    );
    intent.name = Some("mw-abort".into());
    intent.ttl_hours =
        trade_control_core::tunable::Tunable::Static(ttl_hours_until(now, veto_expiry));
    intent.level = Some(VetoLevel::CancelPending);
    BuiltAlert {
        basename: AlertBasename::VetoMwAbort.as_str().into_owned(),
        purpose: "veto: mw-abort (cancel pending if a candle closes back through the neckline)"
            .into(),
        intent,
    }
}

/// `01-veto-mw-overshoot` — `CancelPending`. Fires intra-bar when price
/// crosses the 180%-of-top→neckline level (the move is essentially
/// complete; a late entry's R:R no longer justifies opening). The chart
/// binds this to a computed static price (the arm-time overshoot level);
/// `tv-arm`'s `alert_spec` sets the value + frequency. `CancelPending` not
/// `ClosePositions` — overshoot only matters while the entry is pending; a
/// filled trade rides its own SL/TP.
fn build_mw_overshoot_alert(
    instrument: &str,
    trade_id: &str,
    veto_expiry: DateTime<Utc>,
    broker: &BrokerKind,
    account: &str,
    now: DateTime<Utc>,
) -> BuiltAlert {
    let id = format!("{trade_id}-mw-overshoot");
    let mut intent = skeleton(
        Action::Veto,
        instrument,
        id,
        veto_expiry,
        *broker,
        account,
        trade_id,
    );
    intent.name = Some(MW_OVERSHOOT_VETO_NAME.into());
    intent.ttl_hours =
        trade_control_core::tunable::Tunable::Static(ttl_hours_until(now, veto_expiry));
    intent.level = Some(VetoLevel::CancelPending);
    BuiltAlert {
        basename: AlertBasename::VetoMwOvershoot.as_str().into_owned(),
        purpose: "veto: mw-overshoot (cancel pending if price runs 180% of top→neckline)".into(),
        intent,
    }
}

/// `05-enter` for M / W. A per-bar stop entry carrying the baked `mw`
/// params; the worker derives entry/SL/TP from them + the live shell, so
/// `entry`, `stop_loss`, and `take_profit` are all left `None`. Gated by
/// the three M/W vetos and no preps. `max_retries: 0` — a stop-out is
/// terminal (no re-entry).
#[allow(clippy::too_many_arguments)]
fn build_mw_enter_alert(
    instrument: &str,
    trade_id: &str,
    direction: Direction,
    mw: MwSpec,
    entry_deadline: DateTime<Utc>,
    risk_pct: f64,
    risk_amount: Option<f64>,
    dry_run: bool,
    allow_entry: Option<&str>,
    needs_golden: bool,
    needs_confirmed: bool,
    blackout_close: BlackoutCloseAction,
    broker: &BrokerKind,
    account: &str,
    breakeven: Option<trade_control_core::intent::Breakeven>,
) -> BuiltAlert {
    let id = format!("{trade_id}-enter");
    let mut intent = skeleton(
        Action::Enter,
        instrument,
        id,
        entry_deadline,
        *broker,
        account,
        trade_id,
    );
    intent.direction = Some(direction);
    // entry / stop_loss / take_profit deliberately left None — the worker
    // computes all three from `mw` + the shell OHLC (mid-correct).
    intent.mw = Some(mw.to_params());
    // Carry the same pip on the top-level field so the worker's shared
    // sizing tail (`pip_size_for`) sees the baked value, not its default.
    intent.pip_size = Some(mw.pip_size);
    match risk_amount {
        Some(amount) => {
            intent.risk_amount = Some(trade_control_core::tunable::Tunable::Static(amount))
        }
        None => intent.risk_pct = trade_control_core::tunable::Tunable::Static(risk_pct),
    }
    if dry_run {
        intent.dry_run = Some(true);
    }
    // Single-shot: a stop-out ends the setup. No re-entry, no preps.
    intent.max_retries = trade_control_core::tunable::Tunable::Static(0);
    intent.allow_entry = allow_entry.map(trade_control_core::tunable::Tunable::from_script);
    intent.needs_golden = needs_golden;
    intent.needs_confirmed = needs_confirmed;
    // Market-hours blackout close policy — see build_enter_alert.
    intent.blackout_close = blackout_close;
    intent.requires_preps = Vec::new();
    intent.vetos = vec![
        MW_CANCEL_VETO_NAME.into(),
        "mw-abort".into(),
        MW_OVERSHOOT_VETO_NAME.into(),
        "trade-expiry".into(),
    ];
    // Break-even — same as H&S; the worker resolves the M/W geometry at fill,
    // so the cron's snapshot has a concrete entry/TP to compute the 50% level.
    intent.breakeven = breakeven;
    BuiltAlert {
        basename: AlertBasename::Enter.as_str().into_owned(),
        purpose: "enter: M/W per-bar stop entry (worker-computed geometry; vetoed by \
                  mw-cancel/mw-abort/mw-overshoot/trade-expiry)"
            .into(),
        intent,
    }
}

/// Display label for a [`PriceAnchor`] in prompt text.
fn anchor_label(anchor: PriceAnchor) -> &'static str {
    match anchor {
        PriceAnchor::Close => "close",
        PriceAnchor::High => "high",
        PriceAnchor::Low => "low",
        PriceAnchor::RecentHigh => "recent_high",
        PriceAnchor::RecentLow => "recent_low",
        PriceAnchor::SignalHigh => "signal_high",
        PriceAnchor::SignalLow => "signal_low",
    }
}

fn prompt_instrument(theme: &ColorfulTheme) -> Result<String> {
    let raw: String = Input::with_theme(theme)
        .with_prompt("instrument (e.g. EUR_USD or 71402)")
        .interact_text()
        .map_err(|e| eyre!("instrument prompt: {e}"))?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        return Err(eyre!("instrument is required"));
    }
    Ok(trimmed)
}

fn prompt_broker(theme: &ColorfulTheme) -> Result<BrokerKind> {
    let options = ["oanda", "tradenation"];
    let idx = FuzzySelect::with_theme(theme)
        .with_prompt("broker")
        .items(options)
        .default(0)
        .interact()
        .map_err(|e| eyre!("broker pick aborted: {e}"))?;
    Ok(match idx {
        0 => BrokerKind::Oanda,
        _ => BrokerKind::TradeNation,
    })
}

fn prompt_trade_expiry(
    theme: &ColorfulTheme,
    instrument: &str,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let default_anchor = expiry::load(instrument, now).unwrap_or(now + expiry::DEFAULT_HORIZON);
    let default_str = default_anchor.to_rfc3339();
    let raw: String = Input::with_theme(theme)
        .with_prompt(format!("trade_expiry for {instrument} (RFC3339, UTC)"))
        .default(default_str)
        .interact_text()
        .map_err(|e| eyre!("trade_expiry prompt: {e}"))?;
    let parsed: DateTime<Utc> = raw
        .parse()
        .map_err(|e| eyre!("parse trade_expiry {raw:?}: {e}"))?;
    if parsed <= now {
        return Err(eyre!("trade_expiry must be in the future"));
    }
    Ok(parsed)
}

/// Compute the entry window's `not_after` as a percentage of the span
/// between *now* and `trade_expiry`. Caller clamps the percentage to
/// 1..=100 via the prompt's input validation; here we just apply it.
fn derive_entry_deadline(
    now: DateTime<Utc>,
    trade_expiry: DateTime<Utc>,
    pct: u32,
) -> DateTime<Utc> {
    let span = trade_expiry - now;
    let fraction = (pct.min(100) as i64) * span.num_seconds() / 100;
    now + Duration::seconds(fraction)
}

/// Mint a unique trade_id slug: `<pattern>-<instrument-lower>-<4-byte-hex>`.
/// Stays well under [`trade_control_core::intent::TRADE_ID_MAX_LEN`].
fn mint_trade_id(pattern: TradePattern, instrument: &str) -> Result<String> {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).map_err(|e| eyre!("getrandom: {e}"))?;
    let suffix = hex::encode(bytes);
    let mut instr = String::new();
    for c in instrument.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            instr.push(c);
        } else if !instr.ends_with('-') {
            // Collapse any run of non-alphanumerics (e.g. " (" in
            // "Sugar (No 5)") into a single hyphen — consecutive hyphens
            // are not a valid slug.
            instr.push('-');
        }
    }
    let instr = instr.trim_matches('-');
    let id = format!("{}-{instr}-{suffix}", pattern.slug());
    Ok(id)
}

fn skeleton(
    action: Action,
    instrument: &str,
    id: String,
    not_after: DateTime<Utc>,
    broker: BrokerKind,
    account: &str,
    trade_id: &str,
) -> Intent {
    Intent {
        entry_level_vetos: Vec::new(),
        v: 1,
        id,
        not_before: None,
        not_after,
        action,
        instrument: instrument.to_string(),
        direction: None,
        entry: None,
        stop_loss: None,
        take_profit: None,
        risk_pct: trade_control_core::tunable::Tunable::Static(1.0),
        risk_amount: None,
        size_units: None,
        dry_run: None,
        cooldown_hours: None,
        min_r: None,
        broker,
        step: None,
        name: None,
        ttl_hours: trade_control_core::tunable::Tunable::Static(0),
        level: None,
        requires_preps: Vec::new(),
        vetos: Vec::new(),
        clears: Vec::new(),
        account: Some(account.to_string()),
        trade_id: Some(trade_id.to_string()),
        max_retries: trade_control_core::tunable::Tunable::Static(0),
        expiry_bars: None,
        allow_entry: None,
        allow_close: None,
        needs_golden: false,
        blackout_id: None,
        news_id: None,
        require_news_window: None,
        require_price_in_ranges: None,
        needs_confirmed: false,
        inside_window: Vec::new(),
        sr_bands: Vec::new(),
        veto_on_reversal: false,
        reason: None,
        mw: None,
        pip_size: None,
        trade_plan: None,
        blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
        breakeven: None,
        include_archived: false,
    }
}

/// Builds one of the two geometry-driven price vetos. Named `too-high`
/// for short patterns and `too-low` for long ones — the name comes from
/// the geometry struct, so the wire form matches the reference templates
/// (`too-high.yaml` / `too-low.yaml`).
///
/// `level` is supplied by the caller because the two vetos this builder
/// serves are *not* symmetric in meaning:
///
/// - The **invalidation** veto (price ran back past the right shoulder →
///   structure broken, thesis dead) gets `ClosePositions`: an open trade
///   should be flattened.
/// - The **pcl-exhausted** veto (price already ran most of the way to TP
///   without us in) gets `StopNextEntry`: it only blocks a *late* entry
///   and must never touch an open position — a breach in the trade's
///   favour is profit, not a reason to exit. See
///   `BUG-too-low-closes-positions.md` (demo trade 046).
#[allow(clippy::too_many_arguments)]
fn build_invalidation_alert(
    instrument: &str,
    trade_id: &str,
    veto_name: &str,
    level: VetoLevel,
    veto_expiry: DateTime<Utc>,
    broker: &BrokerKind,
    account: &str,
    now: DateTime<Utc>,
) -> BuiltAlert {
    let id = format!("{trade_id}-{veto_name}");
    let mut intent = skeleton(
        Action::Veto,
        instrument,
        id,
        veto_expiry,
        *broker,
        account,
        trade_id,
    );
    intent.name = Some(veto_name.to_string());
    intent.ttl_hours =
        trade_control_core::tunable::Tunable::Static(ttl_hours_until(now, veto_expiry));
    intent.level = Some(level);
    let basename = match veto_name {
        "too-high" => AlertBasename::VetoTooHigh.as_str().into_owned(),
        "too-low" => AlertBasename::VetoTooLow.as_str().into_owned(),
        // Defensive: PatternGeometry only ever picks too-high/too-low
        // today, so this branch is unreachable in practice. Falling
        // back to the literal format preserves the previous behaviour
        // if a future pattern introduces a new veto label without
        // updating the conventions crate.
        other => format!("01-veto-{other}"),
    };
    let level_note = match level {
        VetoLevel::ClosePositions => "close positions if price runs past invalidation",
        VetoLevel::CancelPending => "cancel pending entry if price runs past invalidation",
        VetoLevel::StopNextEntry => "block a late entry; never touch an open position",
    };
    BuiltAlert {
        basename,
        purpose: format!("veto: {veto_name} ({level_note})"),
        intent,
    }
}

fn build_trade_expiry_alert(
    instrument: &str,
    trade_id: &str,
    trade_expiry: DateTime<Utc>,
    veto_expiry: DateTime<Utc>,
    broker: &BrokerKind,
    account: &str,
    now: DateTime<Utc>,
) -> BuiltAlert {
    let id = format!("{trade_id}-trade-expiry");
    let mut intent = skeleton(
        Action::Veto,
        instrument,
        id,
        veto_expiry,
        *broker,
        account,
        trade_id,
    );
    intent.not_before = Some(trade_expiry);
    intent.name = Some("trade-expiry".into());
    intent.ttl_hours =
        trade_control_core::tunable::Tunable::Static(ttl_hours_until(now, veto_expiry));
    intent.level = Some(VetoLevel::ClosePositions);
    BuiltAlert {
        basename: AlertBasename::VetoTradeExpiry.as_str().into_owned(),
        purpose: "veto: trade-expiry (time-fired close-positions at wall-clock expiry)".into(),
        intent,
    }
}

fn build_break_and_close_alert(
    instrument: &str,
    trade_id: &str,
    trade_expiry: DateTime<Utc>,
    broker: &BrokerKind,
    account: &str,
    now: DateTime<Utc>,
) -> BuiltAlert {
    let id = format!("{trade_id}-break-and-close");
    let mut intent = skeleton(
        Action::Prep,
        instrument,
        id,
        trade_expiry,
        *broker,
        account,
        trade_id,
    );
    intent.step = Some("break-and-close".into());
    intent.ttl_hours =
        trade_control_core::tunable::Tunable::Static(ttl_hours_until(now, trade_expiry));
    // Landing a fresh break-and-close invalidates any stale retest
    // from a prior, abandoned setup on the same instrument.
    intent.clears = vec!["retest".into()];
    BuiltAlert {
        basename: AlertBasename::PrepBreakAndClose.as_str().into_owned(),
        purpose: "prep: break-and-close (close beyond neckline; clears stale retest)".into(),
        intent,
    }
}

fn build_retest_alert(
    instrument: &str,
    trade_id: &str,
    trade_expiry: DateTime<Utc>,
    broker: &BrokerKind,
    account: &str,
    now: DateTime<Utc>,
) -> BuiltAlert {
    let id = format!("{trade_id}-retest");
    let mut intent = skeleton(
        Action::Prep,
        instrument,
        id,
        trade_expiry,
        *broker,
        account,
        trade_id,
    );
    intent.step = Some("retest".into());
    intent.ttl_hours =
        trade_control_core::tunable::Tunable::Static(ttl_hours_until(now, trade_expiry));
    BuiltAlert {
        basename: AlertBasename::PrepRetest.as_str().into_owned(),
        purpose: "prep: retest (price returns to neckline; gates entry)".into(),
        intent,
    }
}

/// Build a `prep-expire` alert for `step`. Drawing-bound on the chart
/// to a `<step>-expiry` vertical line; when crossed, the worker blocks
/// any further `prep` for that step on the trade, so a setup whose prep
/// lands too late (e.g. an H&S break-and-close past the allowed bar
/// count) can never enter. TTL'd to `veto_expiry` like the other
/// time-bounded flags — well past `trade_expiry` so the block outlives
/// the entry window it guards.
fn build_prep_expire_alert(
    instrument: &str,
    trade_id: &str,
    step: &str,
    veto_expiry: DateTime<Utc>,
    broker: &BrokerKind,
    account: &str,
    now: DateTime<Utc>,
) -> BuiltAlert {
    let id = format!("{trade_id}-{step}-expiry");
    let mut intent = skeleton(
        Action::PrepExpire,
        instrument,
        id,
        veto_expiry,
        *broker,
        account,
        trade_id,
    );
    intent.step = Some(step.to_string());
    intent.ttl_hours =
        trade_control_core::tunable::Tunable::Static(ttl_hours_until(now, veto_expiry));
    BuiltAlert {
        basename: AlertBasename::PrepExpire(step.to_string())
            .as_str()
            .into_owned(),
        purpose: format!("prep-expire: block future {step} preps once the cutoff line is crossed"),
        intent,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_enter_alert(
    instrument: &str,
    trade_id: &str,
    geometry: &PatternGeometry,
    entry_deadline: DateTime<Utc>,
    entry_offset_pips: f64,
    sl_offset_pips: f64,
    tp_price: f64,
    sl_price: Option<f64>,
    risk_pct: f64,
    risk_amount: Option<f64>,
    dry_run: bool,
    max_retries: u32,
    expiry_bars: Option<u32>,
    allow_entry: Option<&str>,
    entry_mode: EntryMode,
    needs_golden: bool,
    needs_confirmed: bool,
    skip_preps: &[String],
    pip_size: Option<f64>,
    blackout_close: BlackoutCloseAction,
    broker: &BrokerKind,
    account: &str,
    check_reversal_veto: bool,
    entry_level_vetos: &[trade_control_core::intent::EntryLevelVeto],
    recover_entry: RecoverEntryAction,
    qm: bool,
    breakeven: Option<trade_control_core::intent::Breakeven>,
) -> BuiltAlert {
    // The QM leg (strategy-v2's second enter) gets the `09-enter-qm`
    // basename; every other pattern enter gets `05-enter`. This is keyed
    // off the explicit `qm` flag, *not* the order type: the QM leg is now a
    // `Stop` entry identical to standalone --quasimodo (see the call site),
    // so `entry_mode` no longer distinguishes the two legs.
    let is_qm = qm;
    let basename = if is_qm {
        AlertBasename::EnterQm
    } else {
        AlertBasename::Enter
    };
    let id = if is_qm {
        format!("{trade_id}-enter-qm")
    } else {
        format!("{trade_id}-enter")
    };
    let mut intent = skeleton(
        Action::Enter,
        instrument,
        id,
        entry_deadline,
        *broker,
        account,
        trade_id,
    );
    intent.direction = Some(geometry.direction);
    // Baked pip scales the entry/SL offset_pips at the worker; absent =
    // worker falls back to its secret/default.
    intent.pip_size = pip_size;
    intent.entry = Some(match entry_mode {
        EntryMode::Stop => EntrySpec::Stop {
            from: geometry.entry_anchor,
            offset_pips: entry_offset_pips,
            // Pattern entries resolve against the live shell, not an
            // absolute level — `at` is for the position-tool path only.
            at: None,
            // Wrong-side recovery: `Skip` emits no field (today's drop);
            // `Market`/`Limit` opt in. The slippage bound is derived by the
            // resolver from the SL→entry distance, so none is set here.
            recover_entry: match recover_entry {
                RecoverEntryAction::Skip => None,
                action => Some(RecoverEntry {
                    action,
                    max_slippage_pips: None,
                }),
            },
        },
        EntryMode::Market => EntrySpec::Market,
        // Quasimodo (strategy-v2): a limit resting at the *same* signal
        // anchor the stop entry uses (SignalHigh long / SignalLow short =
        // the latched pattern extreme). A limit there fills on the pullback
        // back to the level rather than a break through it. Offset 0 — rest
        // exactly at the level so the side is unambiguous (a Stop-style
        // offset could land the limit on the wrong side). `at: None` — like
        // the stop, it resolves against the live confirmed-signal shell.
        EntryMode::Limit => EntrySpec::Limit {
            from: geometry.entry_anchor,
            offset_pips: 0.0,
            at: None,
        },
    });
    // SL is normally anchored to the pattern extreme + offset. The
    // position-tool direct entry instead supplies an absolute stop the
    // operator drew, so when `sl_price` is set we emit `Absolute` and
    // ignore the geometry anchor / offset entirely.
    intent.stop_loss = Some(match sl_price {
        Some(absolute) => PriceRef::Absolute { absolute },
        None => PriceRef::Anchored {
            from: geometry.sl_anchor,
            offset_pips: sl_offset_pips,
        },
    });
    // TP is an absolute price the operator typed in — the worker uses
    // it verbatim and ignores the shell.
    intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
        absolute: tp_price,
    }));
    // risk_amount, when set, supersedes risk_pct (which always carries a
    // value post-flatten — Static(1.0) by default). Leaving risk_pct
    // alone in that branch is fine: the worker's sizing-mode selector
    // ignores it when risk_amount is set.
    match risk_amount {
        Some(amount) => {
            intent.risk_amount = Some(trade_control_core::tunable::Tunable::Static(amount))
        }
        None => intent.risk_pct = trade_control_core::tunable::Tunable::Static(risk_pct),
    }
    if dry_run {
        intent.dry_run = Some(true);
    }
    intent.max_retries = trade_control_core::tunable::Tunable::Static(max_retries);
    intent.expiry_bars = expiry_bars.map(trade_control_core::tunable::Tunable::Static);
    intent.allow_entry = allow_entry.map(trade_control_core::tunable::Tunable::from_script);
    intent.needs_golden = needs_golden;
    intent.needs_confirmed = needs_confirmed;
    // Market-hours blackout close policy — what the sweep does with this
    // order if it's caught resting in the close→open gap.
    intent.blackout_close = blackout_close;
    intent.requires_preps = ["break-and-close", "retest"]
        .into_iter()
        .filter(|step| !skip_preps.iter().any(|s| s == step))
        .map(String::from)
        .collect();
    intent.vetos = vec![
        geometry.invalidation_veto_name.into(),
        geometry.pcl_exhausted_veto_name.into(),
        "trade-expiry".into(),
    ];
    // Experimental: when the reversal-close is armed to veto-on-reversal,
    // the enter must also list `reversal` so the veto it writes actually
    // gates this entry (the worker only checks veto names the enter lists).
    if check_reversal_veto {
        intent
            .vetos
            .push(trade_control_core::intent::REVERSAL_VETO_NAME.into());
    }
    // Continuous at-entry level vetos (Bug #12): pcl-exhausted / invalidation
    // levels the worker re-checks against the resolved entry price, so an
    // entry already past the level is rejected even when no cross-event guard
    // fired. Carried only on the enter intent.
    intent.entry_level_vetos = entry_level_vetos.to_vec();
    // Break-even stop management — the live worker's position cron moves the SL
    // to break-even once a candle closes past this fraction of entry→TP. Default
    // on at 50% (see `TradeSpec::breakeven_pct`).
    intent.breakeven = breakeven;
    let purpose = if is_qm {
        "enter: quasimodo limit at signal level, confirmed-candle gated, no preps"
    } else {
        "enter: stop-entry gated by both preps + both vetos"
    };
    BuiltAlert {
        basename: basename.as_str().into_owned(),
        purpose: purpose.into(),
        intent,
    }
}

/// Order type for a position-tool direct entry. Mirrors the tv-arm flag
/// trio (`--market-entry` / `--stop-entry` / `--limit-entry`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEntryKind {
    /// Market order, filled by the worker on receipt at broker price.
    Market,
    /// Pending stop order resting at the drawn entry price.
    Stop,
    /// Pending limit order resting at the drawn entry price.
    Limit,
}

/// Everything needed to build a position-tool direct-entry intent. The
/// SL/TP are absolute prices the operator drew (see `tv-arm`'s
/// `position_trade::resolve_levels`); `entry_price` is the drawn entry
/// anchor, used both as the order's resting price (Stop/Limit) and the
/// signed shell's reference `close` for the worker's geometry/R checks.
#[derive(Debug, Clone)]
pub struct PositionEnterSpec {
    pub instrument: String,
    pub account: String,
    pub broker: BrokerKind,
    pub direction: Direction,
    pub kind: PositionEntryKind,
    /// Drawn entry price (absolute).
    pub entry_price: f64,
    /// Absolute stop-loss price.
    pub stop_loss: f64,
    /// Absolute take-profit price.
    pub take_profit: f64,
    /// Trade-expiry deadline — the enter's `not_after`.
    pub trade_expiry: DateTime<Utc>,
    /// Risk per trade as a home-currency amount; when `None`, uses 1% pct.
    pub risk_amount: Option<f64>,
    /// Pip size baked onto the intent (from instrument-lookup).
    pub pip_size: Option<f64>,
    /// When true, the worker logs the order but doesn't push to broker.
    pub dry_run: bool,
}

/// Build and sign a position-tool direct-entry body, ready to POST
/// straight to the worker (no TradingView). Returns `(trade_id, signed_body)`.
///
/// The entry carries absolute SL/TP (`PriceRef::Absolute`), the chosen
/// order type, the drawn `trade_expiry` as `not_after`, and no preps /
/// pattern vetos — it's a naked manual entry. It's signed with a
/// self-contained shell whose reference `close` is the drawn entry price
/// (`wrap_signed_direct_enter`), so the worker's
/// `stop_loss < close < take_profit` range check and R-multiple are
/// evaluated against the operator's drawn geometry.
///
/// All three order types are supported. `Market` fills on receipt;
/// `Stop` / `Limit` rest at the drawn entry price, expressed as the
/// absolute `EntrySpec::{Stop,Limit}::at` trigger (the worker uses it
/// verbatim — no shell anchor). A long stop must sit above the drawn
/// entry's reference and a long limit below it (and vice-versa for short);
/// the worker's geometry check rejects a wrong-side trigger.
pub fn build_position_enter(
    spec: &PositionEnterSpec,
    key: &[u8; KEY_LEN],
    now: DateTime<Utc>,
) -> Result<(String, String)> {
    // Stop/Limit rest at the operator's drawn entry price — expressed as
    // the absolute `at` trigger so the worker uses it verbatim (the shell
    // anchor + offset path is for the pattern builders). `from`/`offset_pips`
    // are inert when `at` is set but must still parse, so give them a sane
    // anchor. Market needs no trigger.
    let entry = match spec.kind {
        PositionEntryKind::Market => EntrySpec::Market,
        PositionEntryKind::Stop => EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 0.0,
            at: Some(spec.entry_price),
            recover_entry: None,
        },
        PositionEntryKind::Limit => EntrySpec::Limit {
            from: PriceAnchor::Close,
            offset_pips: 0.0,
            at: Some(spec.entry_price),
        },
    };

    // trade_id minting reuses the pattern slug machinery; a position
    // entry isn't an H&S/M/W pattern, so tag it `pos`.
    let trade_id = mint_position_trade_id(&spec.instrument)?;
    let id = format!("{trade_id}-enter");
    let mut intent = skeleton(
        Action::Enter,
        &spec.instrument,
        id,
        spec.trade_expiry,
        spec.broker,
        &spec.account,
        &trade_id,
    );
    intent.direction = Some(spec.direction);
    intent.pip_size = spec.pip_size;
    intent.entry = Some(entry);
    intent.stop_loss = Some(PriceRef::Absolute {
        absolute: spec.stop_loss,
    });
    intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
        absolute: spec.take_profit,
    }));
    match spec.risk_amount {
        Some(amount) => {
            intent.risk_amount = Some(trade_control_core::tunable::Tunable::Static(amount))
        }
        None => intent.risk_pct = trade_control_core::tunable::Tunable::Static(1.0),
    }
    if spec.dry_run {
        intent.dry_run = Some(true);
    }
    // Naked manual entry: no preps, no pattern vetos. The enter's own
    // `not_after` (= trade_expiry) bounds its validity.
    intent
        .validate()
        .map_err(|e| eyre!("invalid position enter intent: {e}"))?;
    let body = wrap_signed_direct_enter(&intent, key, spec.entry_price, now)
        .map_err(|e| eyre!("sign position enter: {e}"))?;
    Ok((trade_id, body))
}

/// Mint a `pos-<instrument>-<rand>` trade_id for a position-tool entry.
fn mint_position_trade_id(instrument: &str) -> Result<String> {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).map_err(|e| eyre!("getrandom: {e}"))?;
    let suffix = hex::encode(bytes);
    let mut instr = String::new();
    for c in instrument.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            instr.push(c);
        } else if !instr.ends_with('-') {
            instr.push('-');
        }
    }
    let instr = instr.trim_matches('-');
    Ok(format!("pos-{instr}-{suffix}"))
}

/// Build the consolidated `06-close-on-reversal` alert: an
/// opposing-direction reversal close that the worker only honours
/// when at least one of the configured contextual windows is active.
/// The Python side wires this YAML to the same `Candle Signals` Pine
/// study as `05-enter` but to the *opposite* direction's
/// `alertcondition` plot — so when a confirming reversal candle
/// prints against the open trade, the worker flattens only if the
/// contextual gate passes.
///
/// Contextual gates (OR-composed via `inside_window`):
///
/// - `news` — an open `news:<trade_id>:*` KV entry.
/// - `price` — current broker price inside at least one
///   `sr_bands` entry.
///
/// At least one of the two must be requested (the caller has
/// already checked).
///
/// Candle-quality gate: defaults to `needs_golden: true`. Operator
/// can flip to `needs_confirmed: true` via the spec field.
#[allow(clippy::too_many_arguments)]
fn build_close_on_reversal_alert(
    instrument: &str,
    trade_id: &str,
    trade_expiry: DateTime<Utc>,
    broker: &BrokerKind,
    account: &str,
    include_news_window: bool,
    sr_bands: Vec<[f64; 2]>,
    needs_confirmed: bool,
    veto_on_reversal: bool,
) -> BuiltAlert {
    let id = format!("{trade_id}-close-on-reversal");
    let mut intent = skeleton(
        Action::Close,
        instrument,
        id,
        trade_expiry,
        *broker,
        account,
        trade_id,
    );
    let mut inside_window: Vec<trade_control_core::intent::EventWindow> = Vec::new();
    if include_news_window {
        inside_window.push(trade_control_core::intent::EventWindow::News);
    }
    let has_sr_bands = !sr_bands.is_empty();
    if has_sr_bands {
        inside_window.push(trade_control_core::intent::EventWindow::Price);
    }
    intent.inside_window = inside_window;
    intent.sr_bands = sr_bands;
    // veto_on_reversal only bites with a price window (sr_bands). Gate it
    // on `has_sr_bands` so a news-only reversal-close can't carry a flag
    // the worker would reject at validate time.
    if veto_on_reversal && has_sr_bands {
        intent.veto_on_reversal = true;
    }
    if needs_confirmed {
        intent.needs_confirmed = true;
    } else {
        intent.needs_golden = true;
    }
    intent.reason = Some(
        match (include_news_window, has_sr_bands) {
            (true, true) => "news-or-price reversal",
            (true, false) => "news-window reversal",
            (false, true) => "support/resistance reversal",
            (false, false) => "reversal", // unreachable — caller gated
        }
        .into(),
    );
    BuiltAlert {
        basename: AlertBasename::CloseOnReversal.as_str().into_owned(),
        purpose: "close: opposing reversal candle, gated on the configured contextual window(s)"
            .into(),
        intent,
    }
}

/// Hours between `now` and `until`, rounded up to the next hour and
/// clamped to at least 1. The worker veto TTL also adds the alert's
/// `not_after - now` tail, so this is just the bare TTL component;
/// erring on the short side is safe.
fn ttl_hours_until(now: DateTime<Utc>, until: DateTime<Utc>) -> u32 {
    let secs = (until - now).num_seconds().max(0);
    let hours = secs.div_euclid(3600);
    let rounded_up = if secs % 3600 == 0 { hours } else { hours + 1 };
    rounded_up.clamp(1, u32::MAX as i64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn parse_arg_accepts_canonical_forms() {
        assert_eq!(TradePattern::parse_arg("hs"), Some(TradePattern::Hs));
        assert_eq!(TradePattern::parse_arg("HS"), Some(TradePattern::Hs));
        assert_eq!(TradePattern::parse_arg("ihs"), Some(TradePattern::Ihs));
        assert_eq!(TradePattern::parse_arg("m"), Some(TradePattern::M));
        assert_eq!(TradePattern::parse_arg("w"), Some(TradePattern::W));
        assert_eq!(TradePattern::parse_arg("xyz"), None);
    }

    #[test]
    fn directions_match_pattern_geometry() {
        // H&S is short; IH&S is long. Wrong here would emit an
        // opposite-direction entry alert. M / W aren't wired up yet —
        // `for_pattern` panics for those, so we don't assert on them.
        assert_eq!(
            PatternGeometry::for_pattern(TradePattern::Hs).direction,
            Direction::Short
        );
        assert_eq!(
            PatternGeometry::for_pattern(TradePattern::Ihs).direction,
            Direction::Long
        );
    }

    #[test]
    fn mint_trade_id_is_valid_slug() {
        // Loop a few times — the random suffix is the only varying part.
        for _ in 0..16 {
            let id = mint_trade_id(TradePattern::Hs, "EUR_USD").unwrap();
            assert!(
                trade_control_core::intent::is_valid_trade_id(&id),
                "minted id {id} failed validation"
            );
            assert!(id.starts_with("hs-eur-usd-"), "got {id}");
        }
    }

    #[test]
    fn mint_trade_id_collapses_punctuation_runs() {
        // TradeNation display names contain spaces and parentheses, e.g.
        // "Sugar (No 5)" — the " (" run must collapse to a single hyphen,
        // not produce "sugar--no-5" which fails slug validation.
        let id = mint_trade_id(TradePattern::Hs, "Sugar (No 5)").unwrap();
        assert!(
            trade_control_core::intent::is_valid_trade_id(&id),
            "minted id {id} failed validation"
        );
        assert!(id.starts_with("hs-sugar-no-5-"), "got {id}");
    }

    #[test]
    fn mint_trade_id_handles_purely_numeric_instrument() {
        // TradeNation market ids are numeric (e.g. 71402 = EUR/USD).
        // Must still produce a valid slug.
        let id = mint_trade_id(TradePattern::M, "71402").unwrap();
        assert!(trade_control_core::intent::is_valid_trade_id(&id));
        assert!(id.starts_with("m-71402-"));
    }

    #[test]
    fn derive_entry_deadline_at_80_pct() {
        let now = ts("2026-05-20T00:00:00Z");
        let trade_expiry = ts("2026-05-25T00:00:00Z"); // 5 days
        let deadline = derive_entry_deadline(now, trade_expiry, 80);
        assert_eq!(deadline, ts("2026-05-24T00:00:00Z"));
    }

    #[test]
    fn derive_entry_deadline_clamps_at_100() {
        let now = ts("2026-05-20T00:00:00Z");
        let trade_expiry = ts("2026-05-25T00:00:00Z");
        // Above 100 is silently clamped to 100 so a typo of "120"
        // doesn't push the deadline past trade_expiry.
        let deadline = derive_entry_deadline(now, trade_expiry, 250);
        assert_eq!(deadline, trade_expiry);
    }

    #[test]
    fn ttl_hours_rounds_up() {
        let now = ts("2026-05-20T00:00:00Z");
        // 1h 30m → 2h (better an extra hour than expiring early).
        assert_eq!(ttl_hours_until(now, ts("2026-05-20T01:30:00Z")), 2);
        // Exactly 4h → 4h, no rounding.
        assert_eq!(ttl_hours_until(now, ts("2026-05-20T04:00:00Z")), 4);
        // Past timestamps clamp to 1h (worker also has its own min TTL).
        assert_eq!(ttl_hours_until(now, ts("2026-05-19T22:00:00Z")), 1);
    }

    #[test]
    fn render_manifest_lists_each_alert_file() {
        // Compose a synthetic trade by hand — avoids needing prompt
        // input — and verify the manifest text includes every alert.
        let now = ts("2026-05-20T00:00:00Z");
        let trade_expiry = ts("2026-05-25T00:00:00Z");
        let geometry = PatternGeometry::for_pattern(TradePattern::Hs);
        let alerts = vec![
            build_invalidation_alert(
                "EUR_USD",
                "hs-eur-usd-abcd",
                "too-high",
                VetoLevel::ClosePositions,
                trade_expiry,
                &BrokerKind::Oanda,
                "demo",
                now,
            ),
            build_enter_alert(
                "EUR_USD",
                "hs-eur-usd-abcd",
                &geometry,
                trade_expiry,
                1.0,
                1.0,
                1.0800,
                None,
                1.0,
                None,
                false,
                0,
                None,
                None,
                EntryMode::Stop,
                false,
                false,
                &[],
                None,
                BlackoutCloseAction::default(),
                &BrokerKind::Oanda,
                "demo",
                false,
                &[],
                RecoverEntryAction::Skip,
                false,
                None,
            ),
        ];
        let trade = BuiltTrade {
            trade_id: "hs-eur-usd-abcd".into(),
            instrument: "EUR_USD".into(),
            trade_expiry,
            alerts,
            spec: TradeSpec {
                pattern: TradePattern::Hs,
                instrument: "EUR_USD".into(),
                account: "demo".into(),
                broker: BrokerKind::Oanda,
                trade_expiry,
                risk_pct: DEFAULT_RISK_PCT,
                risk_amount: None,
                dry_run: false,
                max_retries: 0,
                expiry_bars: None,
                needs_golden: false,
                needs_confirmed: false,
                close_on_news: false,
                sr_reversal_ranges: Vec::new(),
                veto_on_reversal: false,
                needs_confirmed_close: false,
                prep_expiries: Vec::new(),
                skip_preps: Vec::new(),
                entry_offset_pips: Some(1.0),
                sl_offset_pips: Some(1.0),
                tp_price: 1.0500,
                sl_price: None,
                entry_deadline_pct: DEFAULT_ENTRY_DEADLINE_PCT,
                allow_entry: None,
                entry_mode: EntryMode::Stop,
                sl_anchor: None,
                mw: None,
                pip_size: None,
                blackout_close: BlackoutCloseAction::default(),
                entry_level_vetos: Vec::new(),
                recover_entry: RecoverEntryAction::Skip,
                strategy_v2: false,
                breakeven_pct: default_breakeven_pct(),
            },
        };
        let manifest = render_manifest(&trade);
        assert!(manifest.contains("trade_id: hs-eur-usd-abcd"));
        assert!(manifest.contains("01-veto-too-high.yaml"));
        assert!(manifest.contains("05-enter.yaml"));
        assert!(manifest.contains("trade_expiry:"));
    }

    #[test]
    fn hs_enter_matches_short_template_geometry() {
        // The H&S enter alert must mirror the operator's reference
        // `short.yaml`: stop-entry at low+1, SL at high+1, vetoed by
        // `too-high` and `trade-expiry`, requires both preps. The TP
        // is absolute — the operator types it in.
        let geometry = PatternGeometry::for_pattern(TradePattern::Hs);
        let deadline = ts("2026-05-24T00:00:00Z");
        let alert = build_enter_alert(
            "EUR_USD",
            "hs-eur-usd-zzzz",
            &geometry,
            deadline,
            -1.0, // entry: 1 pip below signal_low (short break)
            1.0,  // SL: 1 pip above signal_high
            1.0500,
            None,
            1.0,
            None,
            false,
            0,
            None,
            None,
            EntryMode::Stop,
            false,
            false,
            &[],
            None,
            BlackoutCloseAction::default(),
            &BrokerKind::Oanda,
            "demo",
            false,
            &[],
            RecoverEntryAction::Skip,
            false,
            None,
        );
        assert_eq!(alert.intent.direction, Some(Direction::Short));
        // Entry: signal_low − 1 pip — a sell-stop 1 pip *below* the break (the
        // offset pushes away from the pattern), at the latched pattern level
        // (bug #10 finding A), not the candle wick.
        match &alert.intent.entry {
            Some(EntrySpec::Stop {
                from, offset_pips, ..
            }) => {
                assert_eq!(*from, PriceAnchor::SignalLow);
                assert!((offset_pips - (-1.0)).abs() < 1e-9);
            }
            other => panic!("expected Stop entry, got {other:?}"),
        }
        // SL: signal_high + 1 pip — 1 pip *above* the pattern high, not the
        // triggering candle's own high.
        match &alert.intent.stop_loss {
            Some(PriceRef::Anchored { from, offset_pips }) => {
                assert_eq!(*from, PriceAnchor::SignalHigh);
                assert!((offset_pips - 1.0).abs() < 1e-9);
            }
            other => panic!("expected Anchored SL, got {other:?}"),
        }
        // TP: absolute price the operator typed in.
        match &alert.intent.take_profit {
            Some(TakeProfit::Anchored(PriceRef::Absolute { absolute })) => {
                assert!((absolute - 1.0500).abs() < 1e-9);
            }
            other => panic!("expected absolute TP, got {other:?}"),
        }
        assert_eq!(
            alert.intent.requires_preps,
            vec!["break-and-close".to_string(), "retest".to_string()]
        );
        assert_eq!(
            alert.intent.vetos,
            vec![
                "too-high".to_string(),
                "too-low".to_string(),
                "trade-expiry".to_string()
            ]
        );
        assert_eq!(alert.intent.trade_id.as_deref(), Some("hs-eur-usd-zzzz"));
        assert_eq!(alert.intent.account.as_deref(), Some("demo"));
        alert.intent.validate().unwrap();
    }

    #[test]
    fn qm_enter_matches_standalone_quasimodo_entry_spec() {
        // strategy-v2 Quasimodo enter (the `09-enter-qm` basename, selected
        // by the `qm: true` flag): its entry spec must be *byte-identical* to
        // the one standalone --quasimodo produces for 05-enter — a Stop at
        // signal_low − 1 pip with a `recover_entry: Limit` fallback — NOT the
        // old bare-Limit-at-the-level form. A bare sell-limit resting at the
        // level is geometry-invalid for a short whose price has already pushed
        // below it; the engine rejected it and, with no recover_entry, the
        // whole leg evaporated (demo trade 031). The Stop form rests below
        // market, valid for a short, and fills on the pullback. No preps,
        // confirmed-candle gated, `-enter-qm` id suffix. Same vetos / trade_id
        // as the BCR enter so the retry gate correlates them.
        let geometry = PatternGeometry::for_pattern(TradePattern::Hs);
        let deadline = ts("2026-05-24T00:00:00Z");
        let build = |qm: bool, recover: RecoverEntryAction| {
            build_enter_alert(
                "EUR_USD",
                "hs-eur-usd-zzzz",
                &geometry,
                deadline,
                -1.0,
                1.0,
                1.0500,
                None,
                1.0,
                None,
                false,
                5, // multi-shot: keeps the plan alive so the sibling can cancel
                None,
                None,
                EntryMode::Stop,
                false,
                true, // confirmed-candle gated
                &["break-and-close".to_string(), "retest".to_string()],
                None,
                BlackoutCloseAction::default(),
                &BrokerKind::Oanda,
                "demo",
                false,
                &[],
                recover,
                qm,
                None,
            )
        };
        let alert = build(true, RecoverEntryAction::Limit);
        assert_eq!(alert.basename, "09-enter-qm");
        match &alert.intent.entry {
            Some(EntrySpec::Stop {
                from,
                offset_pips,
                at,
                recover_entry,
            }) => {
                assert_eq!(*from, PriceAnchor::SignalLow);
                assert!(
                    (offset_pips - -1.0).abs() < 1e-9,
                    "QM stop rests 1 pip below the level"
                );
                assert!(at.is_none());
                assert_eq!(
                    recover_entry.as_ref().map(|r| r.action),
                    Some(RecoverEntryAction::Limit),
                    "QM leg carries the wrong-side limit recovery"
                );
            }
            other => panic!("expected Stop entry, got {other:?}"),
        }
        assert!(
            alert.intent.requires_preps.is_empty(),
            "QM enter carries no preps"
        );
        assert!(alert.intent.needs_confirmed);
        assert_eq!(alert.intent.trade_id.as_deref(), Some("hs-eur-usd-zzzz"));
        assert_eq!(alert.intent.id, "hs-eur-usd-zzzz-enter-qm");
        alert.intent.validate().unwrap();

        // The QM leg and the standalone --quasimodo BCR enter (qm: false,
        // same Stop+Limit-recover) must produce identical entry specs — only
        // the basename/id differ. This is the regression guard the bug report
        // asks for: the two can never drift apart again.
        let standalone = build(false, RecoverEntryAction::Limit);
        assert_eq!(standalone.basename, "05-enter");
        assert_eq!(alert.intent.entry, standalone.intent.entry);
    }

    #[test]
    fn enter_carries_entry_level_vetos_onto_the_intent() {
        // Bug #12: the continuous at-entry level vetos handed to
        // build_enter_alert land verbatim on the enter intent (and nowhere
        // else). Empty by default (the existing tests above pass `&[]`).
        use trade_control_core::intent::{EntryLevelVeto, VetoSide};
        let geometry = PatternGeometry::for_pattern(TradePattern::Hs);
        let levels = vec![
            EntryLevelVeto {
                name: "too-low".into(),
                level: 1.0700,
                past: VetoSide::Below,
            },
            EntryLevelVeto {
                name: "too-high".into(),
                level: 1.0950,
                past: VetoSide::Above,
            },
        ];
        let alert = build_enter_alert(
            "EUR_USD",
            "hs-eur-usd-elv",
            &geometry,
            ts("2026-05-24T00:00:00Z"),
            1.0,
            1.0,
            1.0500,
            None,
            1.0,
            None,
            false,
            0,
            None,
            None,
            EntryMode::Stop,
            false,
            false,
            &[],
            None,
            BlackoutCloseAction::default(),
            &BrokerKind::Oanda,
            "demo",
            false,
            &levels,
            RecoverEntryAction::Skip,
            false,
            None,
        );
        assert_eq!(alert.intent.entry_level_vetos, levels);
    }

    #[test]
    fn ihs_enter_matches_long_template_geometry() {
        // IH&S mirrors `long.yaml`: stop-entry at high+1, SL at low+1,
        // vetoed by `too-low` (not `too-high`). Direction flips to
        // Long.
        let geometry = PatternGeometry::for_pattern(TradePattern::Ihs);
        let deadline = ts("2026-05-24T00:00:00Z");
        let alert = build_enter_alert(
            "EUR_USD",
            "ihs-eur-usd-yyyy",
            &geometry,
            deadline,
            1.0,  // entry: 1 pip above signal_high (long break)
            -1.0, // SL: 1 pip below signal_low
            1.1500,
            None,
            1.0,
            None,
            false,
            0,
            None,
            None,
            EntryMode::Stop,
            false,
            false,
            &[],
            None,
            BlackoutCloseAction::default(),
            &BrokerKind::Oanda,
            "demo",
            false,
            &[],
            RecoverEntryAction::Skip,
            false,
            None,
        );
        assert_eq!(alert.intent.direction, Some(Direction::Long));
        // Entry: signal_high + 1 pip — a buy-stop 1 pip *above* the break
        // (mirror of the H&S short — pattern level, not the candle wick; bug
        // #10 finding A).
        match &alert.intent.entry {
            Some(EntrySpec::Stop {
                from, offset_pips, ..
            }) => {
                assert_eq!(*from, PriceAnchor::SignalHigh);
                assert!((offset_pips - 1.0).abs() < 1e-9);
            }
            other => panic!("expected Stop entry, got {other:?}"),
        }
        // SL: signal_low − 1 pip — 1 pip *below* the pattern low.
        match &alert.intent.stop_loss {
            Some(PriceRef::Anchored { from, offset_pips }) => {
                assert_eq!(*from, PriceAnchor::SignalLow);
                assert!((offset_pips - (-1.0)).abs() < 1e-9);
            }
            other => panic!("expected Anchored SL, got {other:?}"),
        }
        match &alert.intent.take_profit {
            Some(TakeProfit::Anchored(PriceRef::Absolute { absolute })) => {
                assert!((absolute - 1.1500).abs() < 1e-9);
            }
            other => panic!("expected absolute TP, got {other:?}"),
        }
        assert_eq!(
            alert.intent.vetos,
            vec![
                "too-low".to_string(),
                "too-high".to_string(),
                "trade-expiry".to_string()
            ]
        );
    }

    #[test]
    fn absolute_sl_price_overrides_anchored_sl() {
        // The position-tool direct entry supplies an absolute stop the
        // operator drew. When `sl_price` is Some, the enter intent must
        // carry `PriceRef::Absolute` and ignore the geometry anchor —
        // TP stays absolute as always.
        let geometry = PatternGeometry::for_pattern(TradePattern::Hs);
        let deadline = ts("2026-05-24T00:00:00Z");
        let alert = build_enter_alert(
            "EUR_USD",
            "pos-eur-usd-abs1",
            &geometry,
            deadline,
            1.0,
            1.0,
            1.0500,
            Some(1.0850),
            1.0,
            None,
            false,
            0,
            None,
            None,
            EntryMode::Market,
            false,
            false,
            &[],
            None,
            BlackoutCloseAction::default(),
            &BrokerKind::Oanda,
            "demo",
            false,
            &[],
            RecoverEntryAction::Skip,
            false,
            None,
        );
        match &alert.intent.stop_loss {
            Some(PriceRef::Absolute { absolute }) => {
                assert!((absolute - 1.0850).abs() < 1e-9, "{absolute}");
            }
            other => panic!("expected Absolute SL, got {other:?}"),
        }
        // Market entry sanity — position-tool market path.
        assert!(matches!(alert.intent.entry, Some(EntrySpec::Market)));
        match &alert.intent.take_profit {
            Some(TakeProfit::Anchored(PriceRef::Absolute { absolute })) => {
                assert!((absolute - 1.0500).abs() < 1e-9);
            }
            other => panic!("expected absolute TP, got {other:?}"),
        }
    }

    fn position_spec(kind: PositionEntryKind) -> PositionEnterSpec {
        PositionEnterSpec {
            instrument: "EUR_USD".into(),
            account: "demo".into(),
            broker: BrokerKind::Oanda,
            direction: Direction::Long,
            kind,
            entry_price: 1.1000,
            stop_loss: 1.0900,
            take_profit: 1.1200,
            trade_expiry: ts("2026-05-25T00:00:00Z"),
            risk_amount: None,
            pip_size: Some(0.0001),
            dry_run: false,
        }
    }

    #[test]
    fn position_market_enter_builds_market_entry() {
        let key = [9u8; KEY_LEN];
        let now = ts("2026-05-24T00:00:00Z");
        let (trade_id, body) =
            build_position_enter(&position_spec(PositionEntryKind::Market), &key, now)
                .expect("build market");
        assert!(trade_id.starts_with("pos-"), "{trade_id}");
        assert!(body.contains(r#""type":"market""#), "{body}");
        // Market carries no `at` trigger.
        assert!(!body.contains(r#""at":"#), "{body}");
    }

    #[test]
    fn position_stop_enter_bakes_absolute_trigger() {
        // Phase 2: --stop-entry rests at the drawn entry as an absolute
        // `at` trigger (long, so a stop above — but the wrong-side guard is
        // skipped for `at`, see the core resolver test).
        let key = [9u8; KEY_LEN];
        let now = ts("2026-05-24T00:00:00Z");
        let (_, body) = build_position_enter(&position_spec(PositionEntryKind::Stop), &key, now)
            .expect("build stop");
        assert!(body.contains(r#""type":"stop""#), "{body}");
        assert!(body.contains(r#""at":1.1"#), "{body}");
    }

    #[test]
    fn position_limit_enter_bakes_absolute_trigger() {
        let key = [9u8; KEY_LEN];
        let now = ts("2026-05-24T00:00:00Z");
        let (_, body) = build_position_enter(&position_spec(PositionEntryKind::Limit), &key, now)
            .expect("build limit");
        assert!(body.contains(r#""type":"limit""#), "{body}");
        assert!(body.contains(r#""at":1.1"#), "{body}");
    }

    #[test]
    fn ihs_invalidation_alert_uses_too_low_name() {
        // The veto name is geometry-driven — a misconfig here would
        // mean the long enter's `vetos: [too-low]` gate never fires
        // because the IH&S invalidation alert sets `too-high`.
        let now = ts("2026-05-20T00:00:00Z");
        let veto_expiry = ts("2026-05-25T00:30:00Z");
        let alert = build_invalidation_alert(
            "EUR_USD",
            "ihs-eur-usd-xxxx",
            "too-low",
            VetoLevel::ClosePositions,
            veto_expiry,
            &BrokerKind::Oanda,
            "demo",
            now,
        );
        assert_eq!(alert.intent.name.as_deref(), Some("too-low"));
        assert_eq!(alert.basename, "01-veto-too-low");
        assert!(alert.purpose.contains("too-low"));
    }

    fn sample_spec(pattern: TradePattern, trade_expiry: DateTime<Utc>) -> TradeSpec {
        TradeSpec {
            pattern,
            instrument: "EUR_USD".into(),
            account: "demo".into(),
            broker: BrokerKind::Oanda,
            trade_expiry,
            risk_pct: 1.0,
            risk_amount: None,
            dry_run: false,
            max_retries: 0,
            expiry_bars: None,
            needs_golden: false,
            needs_confirmed: false,
            close_on_news: false,
            sr_reversal_ranges: Vec::new(),
            veto_on_reversal: false,
            needs_confirmed_close: false,
            prep_expiries: Vec::new(),
            skip_preps: Vec::new(),
            entry_offset_pips: None,
            sl_offset_pips: None,
            tp_price: 1.0500,
            sl_price: None,
            entry_deadline_pct: 80,
            allow_entry: None,
            entry_mode: EntryMode::Stop,
            sl_anchor: None,
            mw: None,
            pip_size: None,
            blackout_close: BlackoutCloseAction::default(),
            entry_level_vetos: Vec::new(),
            recover_entry: RecoverEntryAction::Skip,
            strategy_v2: false,
            breakeven_pct: default_breakeven_pct(),
        }
    }

    #[test]
    fn build_trade_from_spec_strategy_v2_emits_two_enters_sharing_trade_id() {
        // strategy-v2 arms a second enter (the QM leg) alongside the BCR stop
        // enter: 7 alerts = the usual 6 + 09-enter-qm, pushed right after
        // 05-enter. The two enters share the trade_id (so the worker retry
        // gate correlates them) and a non-zero max_retries (so the engine
        // keeps the plan alive after the first fire). The QM enter carries no
        // preps; the BCR enter keeps both preps. Crucially, the QM leg's
        // entry spec is now *identical* to the BCR enter's — a Stop at
        // signal_low − 1 pip with `recover_entry: Limit` — not the old
        // bare-Limit form that the engine rejected as wrong-side (trade 031).
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.strategy_v2 = true;
        spec.max_retries = 5;
        // Standalone --quasimodo opts into the limit recovery; strategy-v2's
        // BCR + QM legs must inherit the same so all three entry specs match.
        spec.recover_entry = RecoverEntryAction::Limit;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        assert_eq!(trade.alerts.len(), 7);
        assert_eq!(trade.alerts[5].basename, "05-enter");
        assert_eq!(trade.alerts[6].basename, "09-enter-qm");

        let stop = &trade.alerts[5];
        let qm = &trade.alerts[6];
        // Same trade_id across both enters.
        assert_eq!(stop.intent.trade_id, qm.intent.trade_id);
        assert!(stop.intent.trade_id.is_some());
        // BCR: Stop entry, both preps.
        assert!(matches!(stop.intent.entry, Some(EntrySpec::Stop { .. })));
        assert_eq!(
            stop.intent.requires_preps,
            vec!["break-and-close".to_string(), "retest".to_string()]
        );
        // QM: Stop entry at signal_low − 1 pip with limit recovery (identical
        // order shape to standalone --quasimodo), no preps, confirmed-gated,
        // multi-shot.
        match &qm.intent.entry {
            Some(EntrySpec::Stop {
                from,
                offset_pips,
                at,
                recover_entry,
            }) => {
                assert_eq!(*from, PriceAnchor::SignalLow);
                assert!((offset_pips - -1.0).abs() < 1e-9);
                assert!(at.is_none());
                assert_eq!(
                    recover_entry.as_ref().map(|r| r.action),
                    Some(RecoverEntryAction::Limit)
                );
            }
            other => panic!("expected Stop QM entry, got {other:?}"),
        }
        // The two legs' entry specs are byte-identical — only basename/id
        // differ. This is the regression guard against re-introducing the
        // trade-031 drift.
        assert_eq!(stop.intent.entry, qm.intent.entry);
        assert!(qm.intent.requires_preps.is_empty());
        assert!(qm.intent.needs_confirmed);
        assert!(matches!(
            qm.intent.max_retries,
            trade_control_core::tunable::Tunable::Static(5)
        ));
    }

    #[test]
    fn build_trade_from_spec_threads_recover_entry_onto_enter_intent() {
        // A market/limit recovery on the spec must land on the enter
        // intent's stop entry; `Skip` emits no field (today's drop).
        let now = ts("2026-05-20T00:00:00Z");

        // Skip (default) → bare stop, no recover_entry.
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        match enter.intent.entry.as_ref().unwrap() {
            EntrySpec::Stop { recover_entry, .. } => assert!(recover_entry.is_none()),
            other => panic!("expected stop entry, got {other:?}"),
        }

        // Limit → opts in, no explicit slippage (resolver derives it).
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.recover_entry = RecoverEntryAction::Limit;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        match enter.intent.entry.as_ref().unwrap() {
            EntrySpec::Stop {
                recover_entry: Some(rec),
                ..
            } => {
                assert_eq!(rec.action, RecoverEntryAction::Limit);
                assert!(rec.max_slippage_pips.is_none());
            }
            other => panic!("expected stop with recovery, got {other:?}"),
        }
        enter.intent.validate().unwrap();
    }

    #[test]
    fn build_trade_from_spec_market_entry_ignores_recover_entry() {
        // A market entry has no EntrySpec::Stop, so recover_entry is moot
        // — the enter must still be a plain Market entry.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.entry_mode = EntryMode::Market;
        spec.recover_entry = RecoverEntryAction::Market;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert!(matches!(
            enter.intent.entry.as_ref().unwrap(),
            EntrySpec::Market
        ));
    }

    #[test]
    fn build_trade_from_spec_emits_six_alerts_for_hs() {
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        // 6 alerts: invalidation (too-high), pcl-exhausted (too-low),
        // trade-expiry, break-and-close, retest, enter.
        assert_eq!(trade.alerts.len(), 6);
        assert_eq!(trade.alerts[0].basename, "01-veto-too-high");
        assert_eq!(trade.alerts[1].basename, "01-veto-too-low");
        assert_eq!(trade.alerts[5].basename, "05-enter");
        // The spec is round-tripped onto the BuiltTrade so write_trade
        // can persist it next to the alerts.
        assert_eq!(trade.spec.pattern, TradePattern::Hs);
    }

    #[test]
    fn build_trade_from_spec_emits_seven_alerts_when_close_on_news() {
        // close_on_news → consolidated 06-close-on-reversal with
        // inside_window = [news] only.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.close_on_news = true;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        assert_eq!(trade.alerts.len(), 7);
        assert_eq!(trade.alerts[6].basename, "06-close-on-reversal");
        let close = &trade.alerts[6].intent;
        assert_eq!(close.action, Action::Close);
        assert_eq!(
            close.inside_window,
            vec![trade_control_core::intent::EventWindow::News]
        );
        assert!(close.sr_bands.is_empty());
        assert!(close.needs_golden);
        assert!(!close.needs_confirmed);
        // Deprecated fields stay absent on the new wire form.
        assert_eq!(close.require_news_window, None);
        assert_eq!(close.require_price_in_ranges, None);
        assert!(close.trade_id.is_some());
        close.validate().expect("close-on-reversal intent valid");
    }

    #[test]
    fn build_trade_from_spec_emits_seven_alerts_when_sr_reversal_ranges_set() {
        // sr_reversal_ranges only → consolidated 06-close-on-reversal
        // with inside_window = [price] + sr_bands populated.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.sr_reversal_ranges = vec![[1.0950, 1.0970], [1.1000, 1.1020]];
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        assert_eq!(trade.alerts.len(), 7);
        assert_eq!(trade.alerts[6].basename, "06-close-on-reversal");
        let close = &trade.alerts[6].intent;
        assert_eq!(close.action, Action::Close);
        assert_eq!(
            close.inside_window,
            vec![trade_control_core::intent::EventWindow::Price]
        );
        assert_eq!(close.sr_bands, vec![[1.0950, 1.0970], [1.1000, 1.1020]]);
        assert!(close.needs_golden);
        assert_eq!(close.require_news_window, None);
        assert_eq!(close.require_price_in_ranges, None);
        assert!(close.trade_id.is_some());
        close.validate().expect("close-on-sr-reversal intent valid");
    }

    #[test]
    fn build_trade_from_spec_emits_one_consolidated_alert_when_both_close_flags_set() {
        // Both flags set: one 06 alert with inside_window = [news, price]
        // (OR-composed by the worker). The split 06/07 alerts are gone.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.close_on_news = true;
        spec.sr_reversal_ranges = vec![[1.0950, 1.0970]];
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        assert_eq!(trade.alerts.len(), 7);
        assert_eq!(trade.alerts[6].basename, "06-close-on-reversal");
        let close = &trade.alerts[6].intent;
        assert_eq!(
            close.inside_window,
            vec![
                trade_control_core::intent::EventWindow::News,
                trade_control_core::intent::EventWindow::Price,
            ]
        );
        assert_eq!(close.sr_bands, vec![[1.0950, 1.0970]]);
        assert!(close.needs_golden);
        assert!(
            !trade
                .alerts
                .iter()
                .any(|a| a.basename == "07-close-on-sr-reversal")
        );
        close.validate().expect("consolidated close intent valid");
    }

    #[test]
    fn build_trade_from_spec_close_uses_needs_confirmed_when_flag_set() {
        // needs_confirmed_close flips the candle-quality gate from
        // golden to confirmed.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.close_on_news = true;
        spec.needs_confirmed_close = true;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let close = &trade.alerts[6].intent;
        assert!(close.needs_confirmed);
        assert!(!close.needs_golden);
        close
            .validate()
            .expect("needs_confirmed close intent valid");
    }

    #[test]
    fn build_trade_from_spec_close_off_by_default_has_no_veto_on_reversal() {
        // Even with sr bands, the experimental flag stays off unless asked.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.sr_reversal_ranges = vec![[1.0950, 1.0970]];
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let close = &trade.alerts[6].intent;
        assert!(!close.veto_on_reversal);
        // The enter (05-enter) must NOT list `reversal` when the flag is off.
        assert_eq!(trade.alerts[5].basename, "05-enter");
        let enter = &trade.alerts[5].intent;
        assert!(!enter.vetos.iter().any(|v| v == "reversal"));
        close.validate().expect("default close intent valid");
    }

    #[test]
    fn build_trade_from_spec_close_sets_veto_on_reversal_when_armed() {
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.sr_reversal_ranges = vec![[1.0950, 1.0970]];
        spec.veto_on_reversal = true;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let close = &trade.alerts[6].intent;
        assert!(close.veto_on_reversal);
        // The paired half: the enter MUST list `reversal` in its vetos, or
        // the veto the worker writes would never gate the entry.
        let enter = &trade.alerts[5].intent;
        assert!(
            enter.vetos.iter().any(|v| v == "reversal"),
            "enter must check the reversal veto when armed, got {:?}",
            enter.vetos
        );
        // Still a valid intent (close + price window present).
        close
            .validate()
            .expect("veto_on_reversal close intent valid");
        enter.validate().expect("enter with reversal veto valid");
    }

    #[test]
    fn build_trade_from_spec_veto_on_reversal_suppressed_without_bands() {
        // News-only reversal-close: no sr bands, so the flag must NOT be
        // emitted (the worker would reject it at validate time) and the
        // enter must NOT list `reversal` (there's no writer for it).
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.close_on_news = true;
        spec.veto_on_reversal = true;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let close = &trade.alerts[6].intent;
        assert!(!close.veto_on_reversal);
        let enter = &trade.alerts[5].intent;
        assert!(!enter.vetos.iter().any(|v| v == "reversal"));
        close
            .validate()
            .expect("news-only close intent valid without veto flag");
    }

    #[test]
    fn build_trade_from_spec_emits_six_alerts_for_ihs() {
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Ihs, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        // IH&S flips the veto direction: invalidation = too-low,
        // pcl-exhausted = too-high.
        assert_eq!(trade.alerts[0].basename, "01-veto-too-low");
        assert_eq!(trade.alerts[1].basename, "01-veto-too-high");
    }

    #[test]
    fn build_trade_from_spec_pcl_exhausted_veto_shares_shape_but_not_level() {
        // The pcl-exhausted veto is built by the same builder as the
        // invalidation veto and shares its action/trade_id (so the chart
        // side can treat them symmetrically — one drawing-bound, one
        // value-bound). But it must NOT share the level: invalidation is
        // ClosePositions (thesis dead → flatten), pcl-exhausted is
        // StopNextEntry (entry-gate only — a breach is in the trade's
        // favour and must never close an open winner). Regression for
        // BUG-too-low-closes-positions.md (demo trade 046).
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let invalidation = &trade.alerts[0];
        let pcl_exhausted = &trade.alerts[1];
        assert_eq!(invalidation.intent.name.as_deref(), Some("too-high"));
        assert_eq!(pcl_exhausted.intent.name.as_deref(), Some("too-low"));
        // Shared shape: same trade_id and same Veto action.
        assert_eq!(invalidation.intent.trade_id, pcl_exhausted.intent.trade_id);
        assert_eq!(invalidation.intent.action, pcl_exhausted.intent.action);
        // Divergent level — the whole point of the fix.
        assert_eq!(invalidation.intent.level, Some(VetoLevel::ClosePositions));
        assert_eq!(pcl_exhausted.intent.level, Some(VetoLevel::StopNextEntry));
        assert_ne!(invalidation.intent.level, pcl_exhausted.intent.level);
    }

    #[test]
    fn pcl_exhausted_veto_never_closes_positions_for_both_patterns() {
        // Regression guard across both configured patterns: the
        // entry-gate (pcl-exhausted) veto is StopNextEntry, the
        // invalidation veto is ClosePositions — regardless of which
        // direction the pattern trades. alerts[0] is always the
        // invalidation veto, alerts[1] the pcl-exhausted one.
        let now = ts("2026-05-20T00:00:00Z");
        for pattern in [TradePattern::Hs, TradePattern::Ihs] {
            let spec = sample_spec(pattern, ts("2026-05-25T00:00:00Z"));
            let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
            assert_eq!(
                trade.alerts[0].intent.level,
                Some(VetoLevel::ClosePositions),
                "{pattern:?}: invalidation veto must be ClosePositions"
            );
            assert_eq!(
                trade.alerts[1].intent.level,
                Some(VetoLevel::StopNextEntry),
                "{pattern:?}: pcl-exhausted veto must be StopNextEntry, never close an open winner"
            );
        }
    }

    #[test]
    fn build_trade_from_spec_rejects_mw_pattern_without_geometry() {
        // M / W now build, but only when the spec carries `mw`. A bare
        // M spec (no path geometry) is rejected — it's normally written
        // by `tv-arm`, not hand-edited.
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::M, ts("2026-05-25T00:00:00Z"));
        let err = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap_err();
        assert!(err.to_string().contains("requires `mw`"), "got {err}");
    }

    #[test]
    fn build_trade_from_spec_rejects_hs_pattern_carrying_mw_geometry() {
        // The inverse guard: an H&S spec must not carry `mw` — that
        // would be a hand-edit mixing two incompatible shapes.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.mw = Some(sample_mw());
        let err = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap_err();
        assert!(err.to_string().contains("must not carry `mw`"), "got {err}");
    }

    /// A well-formed M (short) path geometry for tests. Worked numbers
    /// from `mw_geometry`: A=1.1000, B=1.1200, C=1.1120.
    fn sample_mw() -> MwSpec {
        MwSpec {
            neckline: 1.1120,
            first_point: 1.1200,
            runup_start: 1.1000,
            right_shoulder: None,
            spread_pips: 1.0,
            pip_size: 0.0001,
        }
    }

    fn mw_spec(pattern: TradePattern, trade_expiry: DateTime<Utc>) -> TradeSpec {
        let mut spec = sample_spec(pattern, trade_expiry);
        spec.mw = Some(sample_mw());
        spec
    }

    #[test]
    fn build_mw_emits_exactly_five_alerts() {
        // M / W bundle: mw-cancel, mw-abort, mw-overshoot, trade-expiry,
        // enter. No prep chain, no pcl-exhausted/invalidation vetos, no
        // close-on-reversal.
        let now = ts("2026-05-20T00:00:00Z");
        for pattern in [TradePattern::M, TradePattern::W] {
            let spec = mw_spec(pattern, ts("2026-05-25T00:00:00Z"));
            let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
            assert_eq!(trade.alerts.len(), 5, "{pattern:?}");
            let basenames: Vec<&str> = trade.alerts.iter().map(|a| a.basename.as_str()).collect();
            assert_eq!(
                basenames,
                vec![
                    "01-veto-mw-cancel",
                    "01-veto-mw-abort",
                    "01-veto-mw-overshoot",
                    "02-veto-trade-expiry",
                    "05-enter",
                ],
                "{pattern:?}"
            );
        }
    }

    #[test]
    fn build_mw_rejects_sl_too_close_to_spread() {
        // sample_mw geometry has SL distance ≈ 0.0080 + ~1.5×spread. A 20-pip
        // spread (0.0020) makes the 10× floor 0.020, above the ~0.011 SL
        // distance → reject at build time before any signing.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = mw_spec(TradePattern::M, ts("2026-05-25T00:00:00Z"));
        spec.mw = Some(MwSpec {
            spread_pips: 20.0,
            ..sample_mw()
        });
        let err = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap_err();
        assert!(
            err.to_string().contains("too close to the spread"),
            "got {err}"
        );
    }

    #[test]
    fn build_mw_allows_normal_sl_vs_spread() {
        // sample_mw's default 1-pip spread → SL distance is ~80× the spread,
        // comfortably above the 10× floor → builds fine.
        let now = ts("2026-05-20T00:00:00Z");
        let spec = mw_spec(TradePattern::M, ts("2026-05-25T00:00:00Z"));
        assert!(build_trade_from_spec(spec, now, BuildStrictness::Strict).is_ok());
    }

    #[test]
    fn build_mw_cancel_abort_overshoot_are_cancel_pending() {
        // All three price-level M/W vetos are CancelPending — never
        // ClosePositions. The abort especially: once filled the trade
        // rides its own SL/TP, a neckline reclaim must not flatten an open
        // position. Same for overshoot (it's an entry-gate, not a thesis
        // invalidation).
        let now = ts("2026-05-20T00:00:00Z");
        let spec = mw_spec(TradePattern::M, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        assert_eq!(trade.alerts[0].intent.name.as_deref(), Some("mw-cancel"));
        assert_eq!(trade.alerts[0].intent.level, Some(VetoLevel::CancelPending));
        assert_eq!(trade.alerts[1].intent.name.as_deref(), Some("mw-abort"));
        assert_eq!(trade.alerts[1].intent.level, Some(VetoLevel::CancelPending));
        assert_eq!(trade.alerts[2].intent.name.as_deref(), Some("mw-overshoot"));
        assert_eq!(trade.alerts[2].intent.level, Some(VetoLevel::CancelPending));
    }

    #[test]
    fn build_mw_enter_carries_baked_geometry_and_no_fixed_prices() {
        // The enter intent must carry the baked `mw` params and leave
        // entry/SL/TP None (worker computes them). Direction follows the
        // pattern; vetos are the three M/W ones; no preps; single-shot.
        let now = ts("2026-05-20T00:00:00Z");
        let spec = mw_spec(TradePattern::M, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = &trade.alerts[4].intent;
        assert_eq!(enter.action, Action::Enter);
        assert_eq!(enter.direction, Some(Direction::Short));
        let mw = enter.mw.expect("enter carries mw params");
        assert!((mw.neckline - 1.1120).abs() < 1e-9);
        assert!((mw.first_point - 1.1200).abs() < 1e-9);
        assert!((mw.runup_start - 1.1000).abs() < 1e-9);
        assert!((mw.spread_pips - 1.0).abs() < 1e-9);
        assert!((mw.pip_size - 0.0001).abs() < 1e-9);
        // The same pip is mirrored onto the top-level field for the
        // worker's sizing tail.
        assert_eq!(enter.pip_size, Some(0.0001));
        assert!(enter.entry.is_none());
        assert!(enter.stop_loss.is_none());
        assert!(enter.take_profit.is_none());
        assert!(enter.requires_preps.is_empty());
        assert_eq!(
            enter.vetos,
            vec![
                "mw-cancel".to_string(),
                "mw-abort".to_string(),
                "mw-overshoot".to_string(),
                "trade-expiry".to_string(),
            ]
        );
        assert!(matches!(
            enter.max_retries,
            trade_control_core::tunable::Tunable::Static(0)
        ));
        enter.validate().expect("mw enter intent valid");
    }

    #[test]
    fn build_mw_w_pattern_is_long() {
        // W (double-bottom) is long — the only direction difference from
        // M in the bundle.
        let now = ts("2026-05-20T00:00:00Z");
        let spec = mw_spec(TradePattern::W, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        assert_eq!(trade.alerts[4].intent.direction, Some(Direction::Long));
    }

    #[test]
    fn build_hs_enter_carries_baked_pip_size() {
        // An H&S enter built from a spec with a baked pip_size carries it
        // on the top-level field so the worker scales offset_pips with it
        // instead of its forex default.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.pip_size = Some(0.01); // JPY-scale
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = &trade.alerts[5].intent;
        assert_eq!(enter.action, Action::Enter);
        assert_eq!(enter.pip_size, Some(0.01));
        enter.validate().expect("hs enter with pip valid");
    }

    #[test]
    fn build_hs_enter_omits_pip_size_when_spec_has_none() {
        // No baked pip → no top-level field → worker falls back to its
        // secret/default (pre-feature behaviour).
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        assert_eq!(spec.pip_size, None);
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        assert_eq!(trade.alerts[5].intent.pip_size, None);
    }

    #[test]
    fn build_trade_from_spec_rejects_past_trade_expiry() {
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-19T00:00:00Z"));
        let err = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap_err();
        assert!(err.to_string().contains("future"), "got {err}");
    }

    #[test]
    fn build_trade_from_spec_lenient_allows_past_trade_expiry() {
        // The offline `--plan-out` path (replaying a historical setup) must
        // still build even when trade_expiry has already elapsed — it only
        // warns, so the JSON gets written.
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-19T00:00:00Z"));
        assert!(build_trade_from_spec(spec, now, BuildStrictness::Lenient).is_ok());
    }

    #[test]
    fn build_trade_from_spec_rejects_bad_entry_deadline_pct() {
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.entry_deadline_pct = 0;
        assert!(build_trade_from_spec(spec, now, BuildStrictness::Strict).is_err());
        let mut spec2 = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec2.entry_deadline_pct = 101;
        assert!(build_trade_from_spec(spec2, now, BuildStrictness::Strict).is_err());
    }

    #[test]
    fn build_trade_from_spec_applies_pattern_default_offsets() {
        // Omitting entry/sl offsets must fall back to the pattern's
        // geometry defaults (HS short entry: 1 pip *below* signal_low).
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        match &enter.intent.entry {
            Some(EntrySpec::Stop { offset_pips, .. }) => {
                assert!((offset_pips - (-1.0)).abs() < 1e-9);
            }
            other => panic!("expected Stop entry, got {other:?}"),
        }
    }

    #[test]
    fn trade_spec_minimal_yaml_round_trips_with_defaults() {
        // The minimal YAML an operator should be able to write — only
        // the un-defaulted fields. Everything else must come from the
        // serde defaults.
        let yaml = "\
pattern: hs
instrument: EUR_USD
account: demo
trade_expiry: \"2026-05-25T00:00:00Z\"
tp_price: 1.05
";
        let spec: TradeSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.pattern, TradePattern::Hs);
        assert_eq!(spec.broker, BrokerKind::Oanda);
        assert!((spec.risk_pct - DEFAULT_RISK_PCT).abs() < 1e-9);
        assert_eq!(spec.entry_offset_pips, None);
        assert_eq!(spec.sl_offset_pips, None);
        assert_eq!(spec.entry_deadline_pct, DEFAULT_ENTRY_DEADLINE_PCT);
    }

    #[test]
    fn trade_spec_round_trips_through_yaml() {
        // Full spec → YAML → spec must produce the same logical value.
        let original = sample_spec(TradePattern::Ihs, ts("2026-05-25T00:00:00Z"));
        let s = serde_yaml::to_string(&original).unwrap();
        let parsed: TradeSpec = serde_yaml::from_str(&s).unwrap();
        assert_eq!(parsed.pattern, original.pattern);
        assert_eq!(parsed.instrument, original.instrument);
        assert_eq!(parsed.account, original.account);
        assert_eq!(parsed.broker, original.broker);
        assert_eq!(parsed.trade_expiry, original.trade_expiry);
        assert!((parsed.tp_price - original.tp_price).abs() < 1e-9);
        // `mw` is None on an H&S spec and must elide from the wire form.
        assert!(parsed.mw.is_none());
        assert!(!s.contains("mw:"), "mw must elide when None:\n{s}");
    }

    #[test]
    fn mw_spec_round_trips_through_yaml() {
        // An M spec carrying baked path geometry survives YAML round-trip
        // with every anchor intact — this is the form `tv-arm` writes.
        let original = mw_spec(TradePattern::M, ts("2026-05-25T00:00:00Z"));
        let s = serde_yaml::to_string(&original).unwrap();
        assert!(s.contains("mw:"), "mw must serialise when Some:\n{s}");
        let parsed: TradeSpec = serde_yaml::from_str(&s).unwrap();
        let mw = parsed.mw.expect("mw round-trips");
        assert_eq!(mw, sample_mw());
    }

    #[test]
    fn build_trade_from_spec_threads_risk_amount_onto_enter_intent() {
        // When risk_amount is set, the enter intent must carry it and
        // leave risk_pct unset — the worker rejects both being present.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.risk_amount = Some(5.0);
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        match &enter.intent.risk_amount {
            Some(trade_control_core::tunable::Tunable::Static(a)) => {
                assert!((a - 5.0).abs() < 1e-9);
            }
            other => panic!("expected Static(5.0) risk_amount, got {other:?}"),
        }
        // risk_pct is always present (default Static(1.0)) — the build
        // path leaves it alone when risk_amount is set, and the worker's
        // sizing-mode selector silently overrides it.
        assert!(matches!(
            enter.intent.risk_pct,
            trade_control_core::tunable::Tunable::Static(p) if (p - 1.0).abs() < 1e-9
        ));
        enter.intent.validate().unwrap();
    }

    #[test]
    fn build_trade_from_spec_threads_dry_run_onto_enter_intent() {
        // dry_run on the spec must land only on the enter intent —
        // vetos and preps stay unaffected, since they don't open
        // broker orders.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.dry_run = true;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert_eq!(enter.intent.dry_run, Some(true));
        // Spot-check a non-enter alert: dry_run must be None.
        let veto = &trade.alerts[0];
        assert_eq!(veto.intent.dry_run, None);
    }

    #[test]
    fn build_trade_from_spec_threads_blackout_close_onto_enter_intent() {
        // A non-default close policy on the spec must land on the enter
        // intent (the worker reads it there) and leave the non-enter alerts
        // at the wire default.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.blackout_close = BlackoutCloseAction::CancelAndClose;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert_eq!(
            enter.intent.blackout_close,
            BlackoutCloseAction::CancelAndClose
        );
        enter.intent.validate().unwrap();
        // A veto alert keeps the default — the policy is enter-only.
        let veto = &trade.alerts[0];
        assert_eq!(
            veto.intent.blackout_close,
            BlackoutCloseAction::CancelResting
        );
    }

    #[test]
    fn build_trade_from_spec_default_blackout_close_is_cancel_resting() {
        // The default spec mints an enter at the safe incident-fix default.
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert_eq!(
            enter.intent.blackout_close,
            BlackoutCloseAction::CancelResting
        );
    }

    #[test]
    fn build_trade_from_spec_emits_prep_expire_alert_per_entry() {
        // A prep_expiries entry must produce an `08-prep-expire-<step>`
        // alert with action prep-expire, the right step, and a TTL.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.prep_expiries = vec!["break-and-close".into()];
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let pe = trade
            .alerts
            .iter()
            .find(|a| a.basename == "08-prep-expire-break-and-close")
            .expect("prep-expire alert emitted");
        assert_eq!(pe.intent.action, Action::PrepExpire);
        assert_eq!(pe.intent.step.as_deref(), Some("break-and-close"));
        assert!(matches!(
            pe.intent.ttl_hours,
            trade_control_core::tunable::Tunable::Static(h) if h > 0
        ));
        pe.intent.validate().unwrap();
    }

    #[test]
    fn build_trade_from_spec_rejects_unknown_prep_expiry() {
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.prep_expiries = vec!["nonsense".into()];
        assert!(build_trade_from_spec(spec, now, BuildStrictness::Strict).is_err());
    }

    #[test]
    fn build_trade_from_spec_rejects_expiring_a_skipped_prep() {
        // Can't expire a prep that's also been dropped.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.skip_preps = vec!["retest".into()];
        spec.prep_expiries = vec!["retest".into()];
        assert!(build_trade_from_spec(spec, now, BuildStrictness::Strict).is_err());
    }

    #[test]
    fn build_trade_from_spec_threads_needs_golden_onto_enter_intent() {
        // needs_golden on the spec lands on the enter intent only —
        // vetos and preps stay false, since they don't gate on shell
        // signal data.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.needs_golden = true;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert!(enter.intent.needs_golden);
        enter.intent.validate().unwrap();
        for alert in trade.alerts.iter().take(trade.alerts.len() - 1) {
            assert!(
                !alert.intent.needs_golden,
                "non-enter alert {} carried needs_golden",
                alert.basename
            );
        }
    }

    #[test]
    fn build_trade_from_spec_threads_needs_confirmed_onto_enter_intent() {
        // needs_confirmed on the spec lands on the enter intent only,
        // mirroring needs_golden. Vetos and preps stay false.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.needs_confirmed = true;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert!(enter.intent.needs_confirmed);
        // needs_golden is independent — not set unless asked for.
        assert!(!enter.intent.needs_golden);
        enter.intent.validate().unwrap();
        for alert in trade.alerts.iter().take(trade.alerts.len() - 1) {
            assert!(
                !alert.intent.needs_confirmed,
                "non-enter alert {} carried needs_confirmed",
                alert.basename
            );
        }
    }

    #[test]
    fn build_trade_from_spec_both_candle_gates_can_coexist_on_enter() {
        // golden and confirmed are independent gates — both can ride the
        // enter intent for a stricter "golden AND confirmed" entry.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.needs_golden = true;
        spec.needs_confirmed = true;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert!(enter.intent.needs_golden);
        assert!(enter.intent.needs_confirmed);
        enter.intent.validate().unwrap();
    }

    #[test]
    fn build_trade_from_spec_threads_max_retries_onto_enter_intent() {
        // max_retries on the spec lands on the enter intent only as
        // `Static(N)`. Vetos and preps must carry the default
        // `Static(0)` (single-shot), mirroring the dry_run rule.
        // Intent::validate enforces the trade_id + enter-only rules,
        // so the build call must produce a valid intent.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.max_retries = 3;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        match &enter.intent.max_retries {
            trade_control_core::tunable::Tunable::Static(n) => assert_eq!(*n, 3),
            other => panic!("expected Static(3) max_retries, got {other:?}"),
        }
        enter.intent.validate().unwrap();
        for alert in trade.alerts.iter().take(trade.alerts.len() - 1) {
            assert!(
                matches!(
                    alert.intent.max_retries,
                    trade_control_core::tunable::Tunable::Static(0)
                ),
                "non-enter alert {} carried max_retries",
                alert.basename
            );
        }
    }

    #[test]
    fn build_trade_from_spec_threads_expiry_bars_onto_enter_intent() {
        // expiry_bars on the spec lands on the enter intent only as
        // `Static(N)`. Non-enter alerts (vetos/preps) must NOT carry it
        // — bar-expiry is a property of the pending entry order alone.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.expiry_bars = Some(3);
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        match &enter.intent.expiry_bars {
            Some(trade_control_core::tunable::Tunable::Static(n)) => assert_eq!(*n, 3),
            other => panic!("expected Some(Static(3)) expiry_bars, got {other:?}"),
        }
        enter.intent.validate().unwrap();
        for alert in trade.alerts.iter().take(trade.alerts.len() - 1) {
            assert!(
                alert.intent.expiry_bars.is_none(),
                "non-enter alert {} carried expiry_bars",
                alert.basename
            );
        }
    }

    #[test]
    fn build_trade_from_spec_default_expiry_bars_is_none() {
        // Omitting expiry_bars keeps today's behaviour: the enter intent
        // carries no expiry_bars, so the order rests until trade_expiry.
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert!(enter.intent.expiry_bars.is_none());
    }

    #[test]
    fn build_trade_from_spec_bakes_breakeven_on_enter_by_default() {
        // BUG-replay-no-breakeven-stop-at-50pct: the `05-enter` carries a
        // break-even rule (default 50%) so the worker's position cron can move
        // the stop to break-even. Non-enter alerts must NOT carry it.
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        match &enter.intent.breakeven {
            Some(be) => assert!((be.threshold - 0.5).abs() < 1e-9, "default BE = 50%"),
            None => panic!("enter must carry a default break-even rule"),
        }
        for alert in trade.alerts.iter().take(trade.alerts.len() - 1) {
            assert!(
                alert.intent.breakeven.is_none(),
                "non-enter alert {} must not carry breakeven",
                alert.basename
            );
        }
    }

    #[test]
    fn build_trade_from_spec_breakeven_pct_none_disables_it() {
        // `breakeven_pct: None` in the spec opts out — the enter carries no
        // break-even rule and the stop stays static for the life of the trade.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.breakeven_pct = None;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert!(
            enter.intent.breakeven.is_none(),
            "breakeven_pct: None must disable the rule on the enter"
        );
    }

    #[test]
    fn build_trade_from_spec_custom_breakeven_pct_is_carried() {
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.breakeven_pct = Some(0.7);
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        match &enter.intent.breakeven {
            Some(be) => assert!((be.threshold - 0.7).abs() < 1e-9),
            None => panic!("custom breakeven_pct must be carried"),
        }
    }

    #[test]
    fn build_trade_from_spec_default_entry_mode_is_stop() {
        // Omitting entry_mode preserves today's pending-stop entry —
        // critical for wire compat with all pre-existing spec yamls.
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert!(
            matches!(&enter.intent.entry, Some(EntrySpec::Stop { .. })),
            "expected Stop entry, got {:?}",
            enter.intent.entry
        );
    }

    #[test]
    fn build_trade_from_spec_market_entry_mode_emits_market_entry() {
        // entry_mode: market swaps EntrySpec::Stop for EntrySpec::Market.
        // The SL still anchors to geometry — only the entry-side order
        // type changes.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.entry_mode = EntryMode::Market;
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert!(
            matches!(&enter.intent.entry, Some(EntrySpec::Market)),
            "expected Market entry, got {:?}",
            enter.intent.entry
        );
        // SL geometry untouched.
        assert!(matches!(
            &enter.intent.stop_loss,
            Some(PriceRef::Anchored { .. })
        ));
    }

    #[test]
    fn trade_spec_yaml_parses_entry_mode_market() {
        // Wire form an operator (or the Python tool) writes for a
        // market entry.
        let yaml = "\
pattern: hs
instrument: EUR_USD
account: demo
trade_expiry: \"2026-05-25T00:00:00Z\"
tp_price: 1.05
entry_mode: market
";
        let spec: TradeSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.entry_mode, EntryMode::Market);
    }

    #[test]
    fn build_trade_from_spec_threads_allow_entry_script_onto_enter_intent() {
        // A spec-level `allow_entry` string lands on the enter intent
        // as a `Tunable::Script`. Vetos and preps must not carry it —
        // they don't gate broker orders.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.allow_entry = Some("signal_confirmed".into());
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        match &enter.intent.allow_entry {
            Some(trade_control_core::tunable::Tunable::Script(s)) => {
                assert_eq!(s.source, "signal_confirmed");
            }
            other => panic!("expected Script allow_entry, got {other:?}"),
        }
        for alert in trade.alerts.iter().take(trade.alerts.len() - 1) {
            assert!(
                alert.intent.allow_entry.is_none(),
                "non-enter alert {} carried allow_entry",
                alert.basename
            );
        }
    }

    #[test]
    fn enter_alert_with_allow_entry_script_survives_parse_and_verify() {
        // Regression for the CHF/JPY enter rejection observed 2026-06-02:
        // the signed-body emitter flattens `allow_entry: Tunable::Script(...)`
        // through serde_json::to_string into a flow-mapping wire form
        // (`{"!script":"signal_confirmed"}`) — the worker must accept that
        // shape, not just the YAML `!script` tag form. Without the
        // permissive fallback in Tunable's deserializer, this 400s with
        // "invalid type: map, expected a boolean".
        use trade_control_core::incoming::parse_and_verify;
        let now = ts("2026-06-02T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-06-09T00:00:00Z"));
        spec.allow_entry = Some("signal_confirmed".into());
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert_eq!(enter.basename, "05-enter");
        let key = [9u8; KEY_LEN];
        let signed = wrap_signed_template(&enter.intent, &key).unwrap();
        // Simulate TradingView's substitution of the Pine placeholders.
        // signal_confirmed = 1 means the gate would pass; the value
        // matters less here than the *shape* — we're proving the
        // top-level `allow_entry: {...}` line parses at all.
        let on_wire = signed
            .replace("{{close}}", "203.391")
            .replace("{{high}}", "203.395")
            .replace("{{low}}", "203.380")
            .replace("{{open}}", "203.385")
            .replace("{{time}}", "2026-06-02T15:00:00Z")
            .replace("{{plot(\"signal_high\")}}", "203.395")
            .replace("{{plot(\"signal_low\")}}", "203.380")
            .replace("{{plot(\"signal_range\")}}", "0.015")
            .replace("{{plot(\"signal_start_time\")}}", "1780405200000")
            .replace("{{plot(\"signal_kind\")}}", "1")
            .replace("{{plot(\"signal_golden\")}}", "1")
            .replace("{{plot(\"signal_atr\")}}", "0.012")
            .replace("{{plot(\"signal_confirmed\")}}", "1")
            .replace("{{plot(\"recent_high\")}}", "203.500")
            .replace("{{plot(\"recent_low\")}}", "203.000");
        let verify_now = ts("2026-06-02T15:00:30Z");
        let verified = parse_and_verify(&on_wire, &key, verify_now)
            .unwrap_or_else(|e| panic!("verify enter: {e}\n\nbody was:\n{on_wire}"));
        match &verified.intent.allow_entry {
            Some(trade_control_core::tunable::Tunable::Script(s)) => {
                assert_eq!(s.source, "signal_confirmed");
            }
            other => panic!("expected Script allow_entry after round trip, got {other:?}"),
        }
    }

    #[test]
    fn enter_alert_with_expiry_bars_carries_menu_and_survives_round_trip() {
        // When expiry_bars is set, the signed enter body must carry the
        // next_candle_timestamp_1..5 menu placeholders, and after TV
        // substitutes them (ms epochs) the worker routes them onto the
        // Shell and keeps expiry_bars on the Intent.
        use trade_control_core::incoming::parse_and_verify;
        let now = ts("2026-06-02T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-06-09T00:00:00Z"));
        spec.expiry_bars = Some(3);
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert_eq!(enter.basename, "05-enter");
        let key = [9u8; KEY_LEN];
        let signed = wrap_signed_template(&enter.intent, &key).unwrap();
        assert!(
            signed.contains("next_candle_timestamp_3:"),
            "menu placeholder missing from signed body:\n{signed}"
        );
        let on_wire = signed
            .replace("{{close}}", "203.391")
            .replace("{{high}}", "203.395")
            .replace("{{low}}", "203.380")
            .replace("{{open}}", "203.385")
            .replace("{{time}}", "2026-06-02T15:00:00Z")
            .replace("{{plot(\"signal_high\")}}", "203.395")
            .replace("{{plot(\"signal_low\")}}", "203.380")
            .replace("{{plot(\"signal_range\")}}", "0.015")
            .replace("{{plot(\"signal_start_time\")}}", "1780405200000")
            .replace("{{plot(\"signal_kind\")}}", "1")
            .replace("{{plot(\"signal_golden\")}}", "1")
            .replace("{{plot(\"signal_atr\")}}", "0.012")
            .replace("{{plot(\"signal_confirmed\")}}", "1")
            .replace("{{plot(\"recent_high\")}}", "203.500")
            .replace("{{plot(\"recent_low\")}}", "203.000")
            // Hourly forward bar-closes from 16:00Z..20:00Z (ms epochs).
            .replace("{{plot(\"next_candle_timestamp_1\")}}", "1780416000000")
            .replace("{{plot(\"next_candle_timestamp_2\")}}", "1780419600000")
            .replace("{{plot(\"next_candle_timestamp_3\")}}", "1780423200000")
            .replace("{{plot(\"next_candle_timestamp_4\")}}", "1780426800000")
            .replace("{{plot(\"next_candle_timestamp_5\")}}", "1780430400000");
        let verify_now = ts("2026-06-02T15:00:30Z");
        let verified = parse_and_verify(&on_wire, &key, verify_now)
            .unwrap_or_else(|e| panic!("verify enter: {e}\n\nbody was:\n{on_wire}"));
        assert!(verified.intent.expiry_bars.is_some());
        assert_eq!(
            verified
                .shell
                .next_candle_timestamp(3)
                .unwrap()
                .timestamp_millis(),
            1780423200000
        );
    }

    #[test]
    fn enter_alert_without_expiry_bars_omits_menu() {
        // No expiry_bars → byte-for-byte unchanged: the menu lines must
        // not appear, so trades that don't use the feature don't depend
        // on an indicator that ships the menu plots.
        let now = ts("2026-06-02T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-06-09T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        let key = [9u8; KEY_LEN];
        let signed = wrap_signed_template(&enter.intent, &key).unwrap();
        assert!(
            !signed.contains("next_candle_timestamp"),
            "menu placeholder leaked into a no-expiry enter body:\n{signed}"
        );
    }

    #[test]
    fn build_trade_from_spec_rejects_invalid_allow_entry_script() {
        // Sign-time validation catches a parse error in `allow_entry`
        // before any alert is signed. This is the contract operators
        // rely on when authoring spec yaml.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.allow_entry = Some("if foo {{{ bad".into());
        let err = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap_err();
        assert!(err.to_string().contains("allow_entry"), "got {err}");
    }

    #[test]
    fn trade_spec_yaml_script_tag_parses_into_allow_entry() {
        // YAML with the `!script` tag deserialises as
        // `Option<String>` containing the source. (TradeSpec carries
        // the raw string and the builder wraps it; we want the spec
        // shape to accept the same wire form an operator would write.)
        let yaml = "\
pattern: hs
instrument: EUR_USD
account: demo
trade_expiry: \"2026-05-25T00:00:00Z\"
tp_price: 1.05
allow_entry: \"signal_confirmed\"
";
        let spec: TradeSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.allow_entry.as_deref(), Some("signal_confirmed"));
    }

    #[test]
    fn build_trade_from_spec_default_max_retries_is_static_zero() {
        // Minimal YAML — no max_retries field. After round-tripping
        // through serde and through the build pipeline, the enter
        // intent's max_retries lands as `Static(0)` — the wire-elided
        // single-shot default that's byte-identical to pre-feature
        // intents.
        let yaml = "\
pattern: hs
instrument: EUR_USD
account: demo
trade_expiry: \"2026-05-25T00:00:00Z\"
tp_price: 1.05
";
        let spec: TradeSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.max_retries, 0);
        let now = ts("2026-05-20T00:00:00Z");
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert!(matches!(
            enter.intent.max_retries,
            trade_control_core::tunable::Tunable::Static(0)
        ));
    }

    #[test]
    fn build_trade_from_spec_rejects_non_positive_risk_amount() {
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.risk_amount = Some(0.0);
        assert!(build_trade_from_spec(spec, now, BuildStrictness::Strict).is_err());
        let mut spec2 = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec2.risk_amount = Some(-1.0);
        assert!(build_trade_from_spec(spec2, now, BuildStrictness::Strict).is_err());
    }

    #[test]
    fn skip_preps_drops_break_and_close_alert_and_requirement() {
        // --skip-break-and-close alone: retest still required, only the
        // retest prep alert is emitted, and the entry's requires_preps
        // shrinks to just the retest.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.skip_preps = vec!["break-and-close".into()];
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let basenames: Vec<&str> = trade.alerts.iter().map(|a| a.basename.as_str()).collect();
        assert!(!basenames.contains(&"03-prep-break-and-close"));
        assert!(basenames.contains(&"04-prep-retest"));
        let enter = trade.alerts.last().unwrap();
        assert_eq!(enter.intent.requires_preps, vec!["retest".to_string()]);
    }

    #[test]
    fn skip_preps_drops_both_when_both_listed() {
        // --skip-retest implies --skip-break-and-close (the script
        // already encodes that, but the spec must accept both names
        // and produce a no-prep entry alert).
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.skip_preps = vec!["break-and-close".into(), "retest".into()];
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let basenames: Vec<&str> = trade.alerts.iter().map(|a| a.basename.as_str()).collect();
        assert!(!basenames.contains(&"03-prep-break-and-close"));
        assert!(!basenames.contains(&"04-prep-retest"));
        // 4 alerts left: invalidation, pcl-exhausted, trade-expiry, enter.
        assert_eq!(trade.alerts.len(), 4);
        let enter = trade.alerts.last().unwrap();
        assert!(enter.intent.requires_preps.is_empty());
        enter.intent.validate().unwrap();
    }

    #[test]
    fn skip_preps_rejects_unknown_name() {
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.skip_preps = vec!["bogus".into()];
        let err = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap_err();
        assert!(err.to_string().contains("bogus"), "got {err}");
    }

    #[test]
    fn trade_expiry_alert_is_close_positions_with_not_before() {
        // The veto alert that fires at wall-clock trade_expiry must:
        //   1. close positions (so existing trades exit),
        //   2. not_before == trade_expiry (so a misfire before the
        //      time-of-day can't trigger it),
        //   3. carry a veto-grace TTL past trade_expiry.
        let now = ts("2026-05-20T00:00:00Z");
        let trade_expiry = ts("2026-05-25T00:00:00Z");
        let veto_expiry = trade_expiry + DEFAULT_POST_EXPIRY_GRACE;
        let alert = build_trade_expiry_alert(
            "EUR_USD",
            "hs-eur-usd-aaaa",
            trade_expiry,
            veto_expiry,
            &BrokerKind::Oanda,
            "demo",
            now,
        );
        assert_eq!(alert.intent.level, Some(VetoLevel::ClosePositions));
        assert_eq!(alert.intent.not_before, Some(trade_expiry));
        assert_eq!(alert.intent.not_after, veto_expiry);
        assert_eq!(alert.intent.name.as_deref(), Some("trade-expiry"));
    }

    #[test]
    fn sl_anchor_override_lands_on_enter_intent() {
        // Override the H&S default (PriceAnchor::SignalHigh → latched pattern
        // high) with RecentHigh — the SL price ref on the enter intent must
        // pick up the override.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.sl_anchor = Some(PriceAnchor::RecentHigh);
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        match &enter.intent.stop_loss {
            Some(PriceRef::Anchored { from, .. }) => assert_eq!(*from, PriceAnchor::RecentHigh),
            other => panic!("expected Anchored SL, got {other:?}"),
        }
    }

    #[test]
    fn sl_anchor_rejects_wrong_direction() {
        // Short trade with RecentLow SL is nonsensical (SL would be
        // below the entry, on the wrong side of the trade).
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.sl_anchor = Some(PriceAnchor::RecentLow);
        let err = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap_err();
        assert!(err.to_string().contains("short"), "got {err}");
    }

    #[test]
    fn sl_anchor_accepts_recent_low_for_long() {
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Ihs, ts("2026-05-25T00:00:00Z"));
        spec.sl_anchor = Some(PriceAnchor::RecentLow);
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let enter = trade.alerts.last().unwrap();
        match &enter.intent.stop_loss {
            Some(PriceRef::Anchored { from, .. }) => assert_eq!(*from, PriceAnchor::RecentLow),
            other => panic!("expected Anchored SL, got {other:?}"),
        }
    }

    #[test]
    fn drawing_alerts_emit_no_pine_plot_placeholders() {
        // Regression: vetos and preps fire from drawings, which have no
        // Pine context. If their YAML carried `{{plot("…")}}`, TV would
        // deliver it literally and crash the worker's YAML parser
        // (observed 2026-05-27, 19 rejections/day). Only the enter alert
        // (#05) is bound to a Pine study and may carry plot placeholders.
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now, BuildStrictness::Strict).unwrap();
        let key = [9u8; KEY_LEN];
        for alert in &trade.alerts {
            let is_pine_bound = alert.basename == "05-enter";
            let body = if is_pine_bound {
                wrap_signed_template(&alert.intent, &key).unwrap()
            } else {
                wrap_signed_template_drawing(&alert.intent, &key).unwrap()
            };
            // Every TV-template shell — Pine-bound enter and drawing alike —
            // carries `open: {{open}}` (a built-in, added for M/W body-extreme
            // logic). The M/W `05-enter` is the one that actually consumes it.
            assert!(
                body.contains("open: {{open}}"),
                "alert {} must carry the open placeholder, got: {body}",
                alert.basename
            );
            if is_pine_bound {
                assert!(
                    body.contains("{{plot("),
                    "enter alert should carry plot placeholders, got: {body}"
                );
            } else {
                assert!(
                    !body.contains("{{plot("),
                    "drawing alert {} must not carry plot placeholders, got: {body}",
                    alert.basename
                );
                // Sanity: drawing alerts still carry the universally-
                // substituted shell placeholders.
                assert!(body.contains("close: {{close}}"));
                assert!(body.contains("time: \"{{time}}\""));
            }
        }
    }

    #[test]
    fn trade_spec_yaml_parses_sl_anchor_recent_high() {
        let yaml = "\
pattern: hs
instrument: EUR_USD
account: demo
trade_expiry: \"2026-05-25T00:00:00Z\"
tp_price: 1.05
sl_anchor: recent_high
";
        let spec: TradeSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.sl_anchor, Some(PriceAnchor::RecentHigh));
    }
}
