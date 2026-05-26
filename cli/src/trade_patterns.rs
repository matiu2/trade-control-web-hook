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

use trade_control_core::intent::{
    Action, BrokerKind, Direction, EntrySpec, Intent, PriceAnchor, PriceRef, TakeProfit, VetoLevel,
};
use trade_control_core::sig::KEY_LEN;

use crate::control::wrap_signed_template;
use crate::expiry;

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
    /// shoulder of an inverse H&S).
    invalidation_veto_name: &'static str,
}

impl PatternGeometry {
    fn for_pattern(p: TradePattern) -> Self {
        match p {
            TradePattern::Hs => Self {
                direction: Direction::Short,
                entry_anchor: PriceAnchor::Low,
                entry_offset_default: 1.0,
                sl_anchor: PriceAnchor::High,
                sl_offset_default: 1.0,
                invalidation_veto_name: "too-high",
            },
            TradePattern::Ihs => Self {
                direction: Direction::Long,
                entry_anchor: PriceAnchor::High,
                entry_offset_default: 1.0,
                sl_anchor: PriceAnchor::Low,
                sl_offset_default: 1.0,
                invalidation_veto_name: "too-low",
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
    /// Absent (the default) preserves single-shot behaviour. Rejected
    /// at build time if `Some(0)`; the upper bound and the non-Enter /
    /// missing-trade_id rules are enforced by `Intent::validate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
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
    /// Take-profit absolute price. The worker treats this verbatim and
    /// does not consult the shell.
    pub tp_price: f64,
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
        TradePattern::M | TradePattern::W => Err(eyre!(
            "pattern {} is not yet implemented — only `hs` and `ihs` are wired up so far",
            pattern.label()
        )),
    }
}

/// Build a trade from a pre-filled [`TradeSpec`] with no prompts. Used by
/// the `--from-file` flag on `build-trade`. Validates the spec against
/// the same rules the prompts would enforce, then assembles the alerts.
pub fn build_trade_from_spec(spec: TradeSpec, now: DateTime<Utc>) -> Result<BuiltTrade> {
    match spec.pattern {
        TradePattern::Hs | TradePattern::Ihs => {}
        TradePattern::M | TradePattern::W => {
            return Err(eyre!(
                "pattern {} is not yet implemented — only `hs` and `ihs` are wired up so far",
                spec.pattern.label()
            ));
        }
    }
    if spec.instrument.trim().is_empty() {
        return Err(eyre!("instrument is required"));
    }
    if spec.account.trim().is_empty() {
        return Err(eyre!("account is required"));
    }
    if spec.trade_expiry <= now {
        return Err(eyre!("trade_expiry must be in the future"));
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
    if let Some(0) = spec.max_retries {
        return Err(eyre!("max_retries must be at least 1"));
    }
    for name in &spec.skip_preps {
        if !KNOWN_PREP_NAMES.contains(&name.as_str()) {
            return Err(eyre!(
                "skip_preps name {name:?} is not a known prep; expected one of {KNOWN_PREP_NAMES:?}"
            ));
        }
    }
    let geometry = PatternGeometry::for_pattern(spec.pattern);
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
        let body = wrap_signed_template(&alert.intent, key)
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
        max_retries: None,
        skip_preps: Vec::new(),
        entry_offset_pips: Some(entry_offset_pips),
        sl_offset_pips: Some(sl_offset_pips),
        tp_price,
        entry_deadline_pct,
        allow_entry: None,
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
        build_invalidation_alert(
            &spec.instrument,
            &trade_id,
            geometry.invalidation_veto_name,
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
    alerts.push(build_enter_alert(
        &spec.instrument,
        &trade_id,
        &geometry,
        entry_deadline,
        entry_offset_pips,
        sl_offset_pips,
        spec.tp_price,
        spec.risk_pct,
        spec.risk_amount,
        spec.dry_run,
        spec.max_retries,
        spec.allow_entry.as_deref(),
        &spec.skip_preps,
        &spec.broker,
        &spec.account,
    ));

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

/// Display label for a [`PriceAnchor`] in prompt text.
fn anchor_label(anchor: PriceAnchor) -> &'static str {
    match anchor {
        PriceAnchor::Close => "close",
        PriceAnchor::High => "high",
        PriceAnchor::Low => "low",
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
    let instr = instrument
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
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
        risk_pct: None,
        risk_amount: None,
        size_units: None,
        dry_run: None,
        cooldown_hours: None,
        min_r: None,
        broker,
        step: None,
        name: None,
        ttl_hours: None,
        level: None,
        requires_preps: Vec::new(),
        vetos: Vec::new(),
        clears: Vec::new(),
        account: Some(account.to_string()),
        trade_id: Some(trade_id.to_string()),
        max_retries: None,
        allow_entry: None,
    }
}

/// The "price ran past structure and the setup is dead" veto. Named
/// `too-high` for short patterns and `too-low` for long ones — the
/// name comes from the geometry struct, so the wire form matches the
/// reference templates (`too-high.yaml` / `too-low.yaml`).
fn build_invalidation_alert(
    instrument: &str,
    trade_id: &str,
    veto_name: &str,
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
    intent.ttl_hours = Some(trade_control_core::tunable::Tunable::Static(
        ttl_hours_until(now, veto_expiry),
    ));
    intent.level = Some(VetoLevel::ClosePositions);
    BuiltAlert {
        basename: format!("01-veto-{veto_name}"),
        purpose: format!("veto: {veto_name} (close positions if price runs past invalidation)"),
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
    intent.ttl_hours = Some(trade_control_core::tunable::Tunable::Static(
        ttl_hours_until(now, veto_expiry),
    ));
    intent.level = Some(VetoLevel::ClosePositions);
    BuiltAlert {
        basename: "02-veto-trade-expiry".into(),
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
    intent.ttl_hours = Some(trade_control_core::tunable::Tunable::Static(
        ttl_hours_until(now, trade_expiry),
    ));
    // Landing a fresh break-and-close invalidates any stale retest
    // from a prior, abandoned setup on the same instrument.
    intent.clears = vec!["retest".into()];
    BuiltAlert {
        basename: "03-prep-break-and-close".into(),
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
    intent.ttl_hours = Some(trade_control_core::tunable::Tunable::Static(
        ttl_hours_until(now, trade_expiry),
    ));
    BuiltAlert {
        basename: "04-prep-retest".into(),
        purpose: "prep: retest (price returns to neckline; gates entry)".into(),
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
    risk_pct: f64,
    risk_amount: Option<f64>,
    dry_run: bool,
    max_retries: Option<u32>,
    allow_entry: Option<&str>,
    skip_preps: &[String],
    broker: &BrokerKind,
    account: &str,
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
    intent.direction = Some(geometry.direction);
    intent.entry = Some(EntrySpec::Stop {
        from: geometry.entry_anchor,
        offset_pips: entry_offset_pips,
    });
    intent.stop_loss = Some(PriceRef::Anchored {
        from: geometry.sl_anchor,
        offset_pips: sl_offset_pips,
    });
    // TP is an absolute price the operator typed in — the worker uses
    // it verbatim and ignores the shell.
    intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
        absolute: tp_price,
    }));
    // risk_amount, when set, takes precedence over risk_pct — exactly
    // one is allowed on Intent::Enter, and the validator rejects both.
    match risk_amount {
        Some(amount) => {
            intent.risk_amount = Some(trade_control_core::tunable::Tunable::Static(amount))
        }
        None => intent.risk_pct = Some(trade_control_core::tunable::Tunable::Static(risk_pct)),
    }
    if dry_run {
        intent.dry_run = Some(true);
    }
    intent.max_retries = max_retries.map(trade_control_core::tunable::Tunable::Static);
    intent.allow_entry = allow_entry.map(trade_control_core::tunable::Tunable::from_script);
    intent.requires_preps = ["break-and-close", "retest"]
        .into_iter()
        .filter(|step| !skip_preps.iter().any(|s| s == step))
        .map(String::from)
        .collect();
    intent.vetos = vec![
        geometry.invalidation_veto_name.into(),
        "trade-expiry".into(),
    ];
    BuiltAlert {
        basename: "05-enter".into(),
        purpose: "enter: stop-entry gated by both preps + both vetos".into(),
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
                1.0,
                None,
                false,
                None,
                None,
                &[],
                &BrokerKind::Oanda,
                "demo",
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
                max_retries: None,
                skip_preps: Vec::new(),
                entry_offset_pips: Some(1.0),
                sl_offset_pips: Some(1.0),
                tp_price: 1.0500,
                entry_deadline_pct: DEFAULT_ENTRY_DEADLINE_PCT,
                allow_entry: None,
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
            1.0,
            1.0,
            1.0500,
            1.0,
            None,
            false,
            None,
            None,
            &[],
            &BrokerKind::Oanda,
            "demo",
        );
        assert_eq!(alert.intent.direction, Some(Direction::Short));
        // Entry: low + 1 pip.
        match &alert.intent.entry {
            Some(EntrySpec::Stop { from, offset_pips }) => {
                assert_eq!(*from, PriceAnchor::Low);
                assert!((offset_pips - 1.0).abs() < 1e-9);
            }
            other => panic!("expected Stop entry, got {other:?}"),
        }
        // SL: high + 1 pip — matches short.yaml's tight stop.
        match &alert.intent.stop_loss {
            Some(PriceRef::Anchored { from, offset_pips }) => {
                assert_eq!(*from, PriceAnchor::High);
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
            vec!["too-high".to_string(), "trade-expiry".to_string()]
        );
        assert_eq!(alert.intent.trade_id.as_deref(), Some("hs-eur-usd-zzzz"));
        assert_eq!(alert.intent.account.as_deref(), Some("demo"));
        alert.intent.validate().unwrap();
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
            1.0,
            1.0,
            1.1500,
            1.0,
            None,
            false,
            None,
            None,
            &[],
            &BrokerKind::Oanda,
            "demo",
        );
        assert_eq!(alert.intent.direction, Some(Direction::Long));
        match &alert.intent.entry {
            Some(EntrySpec::Stop { from, offset_pips }) => {
                assert_eq!(*from, PriceAnchor::High);
                assert!((offset_pips - 1.0).abs() < 1e-9);
            }
            other => panic!("expected Stop entry, got {other:?}"),
        }
        match &alert.intent.stop_loss {
            Some(PriceRef::Anchored { from, offset_pips }) => {
                assert_eq!(*from, PriceAnchor::Low);
                assert!((offset_pips - 1.0).abs() < 1e-9);
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
            vec!["too-low".to_string(), "trade-expiry".to_string()]
        );
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
            max_retries: None,
            skip_preps: Vec::new(),
            entry_offset_pips: None,
            sl_offset_pips: None,
            tp_price: 1.0500,
            entry_deadline_pct: 80,
            allow_entry: None,
        }
    }

    #[test]
    fn build_trade_from_spec_emits_five_alerts_for_hs() {
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now).unwrap();
        assert_eq!(trade.alerts.len(), 5);
        assert_eq!(trade.alerts[0].basename, "01-veto-too-high");
        assert_eq!(trade.alerts[4].basename, "05-enter");
        // The spec is round-tripped onto the BuiltTrade so write_trade
        // can persist it next to the alerts.
        assert_eq!(trade.spec.pattern, TradePattern::Hs);
    }

    #[test]
    fn build_trade_from_spec_emits_five_alerts_for_ihs() {
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Ihs, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now).unwrap();
        // IH&S → too-low veto (not too-high).
        assert_eq!(trade.alerts[0].basename, "01-veto-too-low");
    }

    #[test]
    fn build_trade_from_spec_rejects_unimplemented_pattern() {
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::M, ts("2026-05-25T00:00:00Z"));
        let err = build_trade_from_spec(spec, now).unwrap_err();
        assert!(err.to_string().contains("not yet implemented"), "got {err}");
    }

    #[test]
    fn build_trade_from_spec_rejects_past_trade_expiry() {
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-19T00:00:00Z"));
        let err = build_trade_from_spec(spec, now).unwrap_err();
        assert!(err.to_string().contains("future"), "got {err}");
    }

    #[test]
    fn build_trade_from_spec_rejects_bad_entry_deadline_pct() {
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.entry_deadline_pct = 0;
        assert!(build_trade_from_spec(spec, now).is_err());
        let mut spec2 = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec2.entry_deadline_pct = 101;
        assert!(build_trade_from_spec(spec2, now).is_err());
    }

    #[test]
    fn build_trade_from_spec_applies_pattern_default_offsets() {
        // Omitting entry/sl offsets must fall back to the pattern's
        // geometry defaults (1 pip from short.yaml / long.yaml).
        let now = ts("2026-05-20T00:00:00Z");
        let spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        let trade = build_trade_from_spec(spec, now).unwrap();
        let enter = trade.alerts.last().unwrap();
        match &enter.intent.entry {
            Some(EntrySpec::Stop { offset_pips, .. }) => {
                assert!((offset_pips - 1.0).abs() < 1e-9);
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
    }

    #[test]
    fn build_trade_from_spec_threads_risk_amount_onto_enter_intent() {
        // When risk_amount is set, the enter intent must carry it and
        // leave risk_pct unset — the worker rejects both being present.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.risk_amount = Some(5.0);
        let trade = build_trade_from_spec(spec, now).unwrap();
        let enter = trade.alerts.last().unwrap();
        match &enter.intent.risk_amount {
            Some(trade_control_core::tunable::Tunable::Static(a)) => {
                assert!((a - 5.0).abs() < 1e-9);
            }
            other => panic!("expected Static(5.0) risk_amount, got {other:?}"),
        }
        assert!(enter.intent.risk_pct.is_none());
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
        let trade = build_trade_from_spec(spec, now).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert_eq!(enter.intent.dry_run, Some(true));
        // Spot-check a non-enter alert: dry_run must be None.
        let veto = &trade.alerts[0];
        assert_eq!(veto.intent.dry_run, None);
    }

    #[test]
    fn build_trade_from_spec_threads_max_retries_onto_enter_intent() {
        // max_retries on the spec lands on the enter intent only —
        // vetos and preps must stay None, mirroring the dry_run rule.
        // Intent::validate enforces the upper bound + trade_id + enter-
        // only rules, so the build call must produce a valid intent.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.max_retries = Some(3);
        let trade = build_trade_from_spec(spec, now).unwrap();
        let enter = trade.alerts.last().unwrap();
        match &enter.intent.max_retries {
            Some(trade_control_core::tunable::Tunable::Static(n)) => assert_eq!(*n, 3),
            other => panic!("expected Static(3) max_retries, got {other:?}"),
        }
        enter.intent.validate().unwrap();
        for alert in trade.alerts.iter().take(trade.alerts.len() - 1) {
            assert!(
                alert.intent.max_retries.is_none(),
                "non-enter alert {} carried max_retries",
                alert.basename
            );
        }
    }

    #[test]
    fn build_trade_from_spec_threads_allow_entry_script_onto_enter_intent() {
        // A spec-level `allow_entry` string lands on the enter intent
        // as a `Tunable::Script`. Vetos and preps must not carry it —
        // they don't gate broker orders.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.allow_entry = Some("signal_confirmed".into());
        let trade = build_trade_from_spec(spec, now).unwrap();
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
    fn build_trade_from_spec_rejects_invalid_allow_entry_script() {
        // Sign-time validation catches a parse error in `allow_entry`
        // before any alert is signed. This is the contract operators
        // rely on when authoring spec yaml.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.allow_entry = Some("if foo {{{ bad".into());
        let err = build_trade_from_spec(spec, now).unwrap_err();
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
    fn build_trade_from_spec_rejects_zero_max_retries() {
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.max_retries = Some(0);
        let err = build_trade_from_spec(spec, now).unwrap_err();
        assert!(err.to_string().contains("max_retries"), "got {err}");
    }

    #[test]
    fn build_trade_from_spec_default_max_retries_is_none() {
        // Minimal YAML — no max_retries field. After round-tripping
        // through serde and through the build pipeline, the enter
        // intent's max_retries stays None (single-shot is the default).
        let yaml = "\
pattern: hs
instrument: EUR_USD
account: demo
trade_expiry: \"2026-05-25T00:00:00Z\"
tp_price: 1.05
";
        let spec: TradeSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.max_retries, None);
        let now = ts("2026-05-20T00:00:00Z");
        let trade = build_trade_from_spec(spec, now).unwrap();
        let enter = trade.alerts.last().unwrap();
        assert_eq!(enter.intent.max_retries, None);
    }

    #[test]
    fn build_trade_from_spec_rejects_non_positive_risk_amount() {
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.risk_amount = Some(0.0);
        assert!(build_trade_from_spec(spec, now).is_err());
        let mut spec2 = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec2.risk_amount = Some(-1.0);
        assert!(build_trade_from_spec(spec2, now).is_err());
    }

    #[test]
    fn skip_preps_drops_break_and_close_alert_and_requirement() {
        // --skip-break-and-close alone: retest still required, only the
        // retest prep alert is emitted, and the entry's requires_preps
        // shrinks to just the retest.
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.skip_preps = vec!["break-and-close".into()];
        let trade = build_trade_from_spec(spec, now).unwrap();
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
        let trade = build_trade_from_spec(spec, now).unwrap();
        let basenames: Vec<&str> = trade.alerts.iter().map(|a| a.basename.as_str()).collect();
        assert!(!basenames.contains(&"03-prep-break-and-close"));
        assert!(!basenames.contains(&"04-prep-retest"));
        // 3 alerts left: invalidation, trade-expiry, enter.
        assert_eq!(trade.alerts.len(), 3);
        let enter = trade.alerts.last().unwrap();
        assert!(enter.intent.requires_preps.is_empty());
        enter.intent.validate().unwrap();
    }

    #[test]
    fn skip_preps_rejects_unknown_name() {
        let now = ts("2026-05-20T00:00:00Z");
        let mut spec = sample_spec(TradePattern::Hs, ts("2026-05-25T00:00:00Z"));
        spec.skip_preps = vec!["bogus".into()];
        let err = build_trade_from_spec(spec, now).unwrap_err();
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
}
