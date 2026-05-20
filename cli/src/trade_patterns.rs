//! `build-trade` — questionnaire-driven multi-alert trade emission.
//!
//! A *trade pattern* (H&S, IH&S, M-top, W-bottom) is a fixed set of 5
//! alerts that share a `trade_id`, a `trade_expiry` anchor, and a single
//! direction. Each alert is independent on the wire — the worker has no
//! notion of "trades" — but the CLI groups them so the operator answers
//! one questionnaire and gets five signed YAMLs to drop into
//! TradingView's alert dialogs.
//!
//! Layout: this file holds the orchestration, the pattern enum, and the
//! H&S implementation. New patterns add a constructor here and a
//! per-pattern build method. IH&S / M / W are stubbed and emit a clear
//! "not yet implemented" so the picker doesn't lie.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use color_eyre::eyre::{Context, Result, eyre};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{FuzzySelect, Input};

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
const DEFAULT_RISK_PCT: f64 = 0.5;

/// Default TP as an R-multiple of stop distance. H&S textbook target is
/// "head-to-neckline distance projected from neckline" — usually
/// 2-3R. We default to 2.0 and let the operator override.
const DEFAULT_TP_R: f64 = 2.0;

/// Default stop-loss offset in pips from the entry anchor. Operator
/// overrides; this just keeps the prompt from being a cold-start blank.
const DEFAULT_SL_OFFSET_PIPS: f64 = 2.0;

/// Default entry stop-trigger offset in pips from the close anchor.
const DEFAULT_ENTRY_OFFSET_PIPS: f64 = 0.0;

/// The catalogue of supported trade patterns. The discriminant doubles
/// as the CLI argument (`hs`, `ihs`, `m`, `w`) and the label in the
/// fuzzy picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    /// The fixed direction this pattern trades.
    fn direction(self) -> Direction {
        match self {
            Self::Hs | Self::M => Direction::Short,
            Self::Ihs | Self::W => Direction::Long,
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
pub struct BuiltAlert {
    pub basename: String,
    /// Human-readable purpose for the manifest. e.g. "veto: too-high
    /// (close-positions)". Operators see this when wiring up TV.
    pub purpose: String,
    pub intent: Intent,
}

/// Outputs of a build-trade run, before they're flushed to disk.
pub struct BuiltTrade {
    pub trade_id: String,
    pub instrument: String,
    pub trade_expiry: DateTime<Utc>,
    pub alerts: Vec<BuiltAlert>,
}

/// Run the full questionnaire for a pattern and return the assembled
/// trade. Output dir is not touched here — see [`write_trade`].
pub fn build_trade_interactive(pattern: TradePattern, now: DateTime<Utc>) -> Result<BuiltTrade> {
    match pattern {
        TradePattern::Hs => build_hs(now),
        TradePattern::Ihs | TradePattern::M | TradePattern::W => Err(eyre!(
            "pattern {} is not yet implemented — only `hs` is wired up so far",
            pattern.label()
        )),
    }
}

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

// ===== H&S pattern =====

fn build_hs(now: DateTime<Utc>) -> Result<BuiltTrade> {
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
        .with_prompt("entry stop trigger offset (pips from close, +ve = above)")
        .default(DEFAULT_ENTRY_OFFSET_PIPS)
        .interact_text()
        .map_err(|e| eyre!("entry offset prompt: {e}"))?;

    let sl_offset_pips: f64 = Input::with_theme(&theme)
        .with_prompt("stop-loss offset (pips from high, +ve = above)")
        .default(DEFAULT_SL_OFFSET_PIPS)
        .interact_text()
        .map_err(|e| eyre!("sl prompt: {e}"))?;

    let tp_r: f64 = Input::with_theme(&theme)
        .with_prompt("take-profit (R-multiple of stop distance)")
        .default(DEFAULT_TP_R)
        .interact_text()
        .map_err(|e| eyre!("tp prompt: {e}"))?;

    let entry_deadline_pct: u32 = Input::with_theme(&theme)
        .with_prompt("entry window ends at (% of time to trade_expiry)")
        .default(DEFAULT_ENTRY_DEADLINE_PCT)
        .interact_text()
        .map_err(|e| eyre!("entry deadline prompt: {e}"))?;

    let trade_id = mint_trade_id(TradePattern::Hs, &instrument)?;
    let entry_deadline = derive_entry_deadline(now, trade_expiry, entry_deadline_pct);
    let veto_expiry = trade_expiry + DEFAULT_POST_EXPIRY_GRACE;

    let direction = TradePattern::Hs.direction();
    let alerts = vec![
        build_too_high_alert(&instrument, &trade_id, veto_expiry, &broker, &account, now),
        build_trade_expiry_alert(
            &instrument,
            &trade_id,
            trade_expiry,
            veto_expiry,
            &broker,
            &account,
            now,
        ),
        build_break_and_close_alert(&instrument, &trade_id, trade_expiry, &broker, &account, now),
        build_retest_alert(&instrument, &trade_id, trade_expiry, &broker, &account, now),
        build_enter_alert(
            &instrument,
            &trade_id,
            direction,
            entry_deadline,
            entry_offset_pips,
            sl_offset_pips,
            tp_r,
            risk_pct,
            &broker,
            &account,
            now,
        ),
    ];

    Ok(BuiltTrade {
        trade_id,
        instrument,
        trade_expiry,
        alerts,
    })
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
    }
}

fn build_too_high_alert(
    instrument: &str,
    trade_id: &str,
    veto_expiry: DateTime<Utc>,
    broker: &BrokerKind,
    account: &str,
    now: DateTime<Utc>,
) -> BuiltAlert {
    let id = format!("{trade_id}-too-high");
    let mut intent = skeleton(
        Action::Veto,
        instrument,
        id,
        veto_expiry,
        *broker,
        account,
        trade_id,
    );
    intent.name = Some("too-high".into());
    intent.ttl_hours = Some(ttl_hours_until(now, veto_expiry));
    intent.level = Some(VetoLevel::ClosePositions);
    BuiltAlert {
        basename: "01-veto-too-high".into(),
        purpose: "veto: too-high (close positions if price runs past invalidation)".into(),
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
    intent.ttl_hours = Some(ttl_hours_until(now, veto_expiry));
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
    intent.ttl_hours = Some(ttl_hours_until(now, trade_expiry));
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
    intent.ttl_hours = Some(ttl_hours_until(now, trade_expiry));
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
    direction: Direction,
    entry_deadline: DateTime<Utc>,
    entry_offset_pips: f64,
    sl_offset_pips: f64,
    tp_r: f64,
    risk_pct: f64,
    broker: &BrokerKind,
    account: &str,
    _now: DateTime<Utc>,
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
    intent.entry = Some(EntrySpec::Stop {
        from: PriceAnchor::Close,
        offset_pips: entry_offset_pips,
    });
    let (sl_anchor, tp_anchor) = match direction {
        Direction::Long => (PriceAnchor::Low, PriceAnchor::Close),
        Direction::Short => (PriceAnchor::High, PriceAnchor::Close),
    };
    intent.stop_loss = Some(PriceRef::Anchored {
        from: sl_anchor,
        offset_pips: sl_offset_pips,
    });
    intent.take_profit = Some(TakeProfit::RMultiple {
        from: tp_anchor,
        offset_r: tp_r,
    });
    intent.risk_pct = Some(risk_pct);
    intent.requires_preps = vec!["break-and-close".into(), "retest".into()];
    intent.vetos = vec!["too-high".into(), "trade-expiry".into()];
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
        // H&S and M are short; IH&S and W are long. Wrong here would
        // emit an opposite-direction entry alert.
        assert_eq!(TradePattern::Hs.direction(), Direction::Short);
        assert_eq!(TradePattern::M.direction(), Direction::Short);
        assert_eq!(TradePattern::Ihs.direction(), Direction::Long);
        assert_eq!(TradePattern::W.direction(), Direction::Long);
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
        let alerts = vec![
            build_too_high_alert(
                "EUR_USD",
                "hs-eur-usd-abcd",
                trade_expiry,
                &BrokerKind::Oanda,
                "demo",
                now,
            ),
            build_enter_alert(
                "EUR_USD",
                "hs-eur-usd-abcd",
                Direction::Short,
                trade_expiry,
                0.0,
                2.0,
                2.0,
                0.5,
                &BrokerKind::Oanda,
                "demo",
                now,
            ),
        ];
        let trade = BuiltTrade {
            trade_id: "hs-eur-usd-abcd".into(),
            instrument: "EUR_USD".into(),
            trade_expiry,
            alerts,
        };
        let manifest = render_manifest(&trade);
        assert!(manifest.contains("trade_id: hs-eur-usd-abcd"));
        assert!(manifest.contains("01-veto-too-high.yaml"));
        assert!(manifest.contains("05-enter.yaml"));
        assert!(manifest.contains("trade_expiry:"));
    }

    #[test]
    fn enter_alert_carries_gates_and_trade_id() {
        // The enter alert is the only one with both preps + vetos
        // wired up, plus the trade_id stamped through.
        let now = ts("2026-05-20T00:00:00Z");
        let deadline = ts("2026-05-24T00:00:00Z");
        let alert = build_enter_alert(
            "EUR_USD",
            "hs-eur-usd-zzzz",
            Direction::Short,
            deadline,
            0.0,
            2.0,
            2.0,
            0.5,
            &BrokerKind::Oanda,
            "demo",
            now,
        );
        assert_eq!(alert.intent.direction, Some(Direction::Short));
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
