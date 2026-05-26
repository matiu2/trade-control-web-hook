//! Helpers for building `status` / `unlock` control intents and signed
//! bodies from CLI args.
//!
//! TradingView is not in the loop for these — the CLI POSTs them
//! directly. The shell fields are still required by the worker's
//! parser, so we fill them with concrete zero values plus a real
//! timestamp.

use chrono::{DateTime, Duration, Utc};
use color_eyre::eyre::{Result, eyre};

use trade_control_core::intent::{Action, BrokerKind, Intent, VetoLevel};
use trade_control_core::sig::KEY_LEN;

/// How long a control envelope stays valid. Short — these are one-shot
/// commands run by hand, so we don't need a long replay window.
const CONTROL_TTL: Duration = Duration::minutes(5);

/// Placeholder instrument string used in `status` envelopes. The action
/// ignores the field, but `ALWAYS_REQUIRED` insists it be present.
const STATUS_INSTRUMENT: &str = "ALL";

/// Skeleton control `Intent` — fills the always-required fields plus
/// `Action` / `instrument` / `id`, leaves every optional field empty.
/// Specific builders override only what they care about.
fn control_skeleton(action: Action, instrument: &str, id: String, now: DateTime<Utc>) -> Intent {
    Intent {
        v: 1,
        id,
        not_before: None,
        not_after: now + CONTROL_TTL,
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
        broker: BrokerKind::Oanda,
        step: None,
        name: None,
        ttl_hours: None,
        level: None,
        requires_preps: Vec::new(),
        vetos: Vec::new(),
        clears: Vec::new(),
        account: None,
        trade_id: None,
        max_retries: None,
        allow_entry: None,
    }
}

/// Build a status `Intent`. `suffix` is a short random tag appended to the
/// id so two concurrent status calls don't collide on replay protection.
pub fn build_status_intent(now: DateTime<Utc>, suffix: &str) -> Intent {
    let id = format!("status-{}-{suffix}", now.format("%Y-%m-%dT%H%M%S"));
    control_skeleton(Action::Status, STATUS_INSTRUMENT, id, now)
}

/// Build an unlock `Intent` for a single instrument.
pub fn build_unlock_intent(instrument: &str, now: DateTime<Utc>, suffix: &str) -> Intent {
    let id = format!(
        "unlock-{instrument}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    control_skeleton(Action::Unlock, instrument, id, now)
}

/// Build a `prep` Intent for a single (instrument, step) pair with a TTL.
///
/// `clears` is the list of other prep steps to drop before recording
/// this one — used to encode ordered prep sequences where landing an
/// upstream step must invalidate stale downstream preps.
pub fn build_prep_intent(
    instrument: &str,
    step: &str,
    ttl_hours: u32,
    clears: Vec<String>,
    now: DateTime<Utc>,
    suffix: &str,
) -> Intent {
    let id = format!(
        "prep-{instrument}-{step}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::Prep, instrument, id, now);
    intent.step = Some(step.to_string());
    intent.ttl_hours = Some(ttl_hours);
    intent.clears = clears;
    intent
}

/// Build a `veto` Intent for a single (instrument, name) pair with a TTL.
/// `level` controls broker side effects at fire time; pass `None` for the
/// default flag-only behaviour ([`VetoLevel::StopNextEntry`]). `clears`
/// lists other vetos to drop before recording this one.
pub fn build_veto_intent(
    instrument: &str,
    name: &str,
    ttl_hours: u32,
    level: Option<VetoLevel>,
    clears: Vec<String>,
    now: DateTime<Utc>,
    suffix: &str,
) -> Intent {
    let id = format!(
        "veto-{instrument}-{name}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::Veto, instrument, id, now);
    intent.name = Some(name.to_string());
    intent.ttl_hours = Some(ttl_hours);
    intent.level = level;
    intent.clears = clears;
    intent
}

/// Build a `clear-prep` Intent for a single (instrument, step) pair.
pub fn build_clear_prep_intent(
    instrument: &str,
    step: &str,
    now: DateTime<Utc>,
    suffix: &str,
) -> Intent {
    let id = format!(
        "clear-prep-{instrument}-{step}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::ClearPrep, instrument, id, now);
    intent.step = Some(step.to_string());
    intent
}

/// Build a `clear-veto` Intent for a single (instrument, name) pair.
pub fn build_clear_veto_intent(
    instrument: &str,
    name: &str,
    now: DateTime<Utc>,
    suffix: &str,
) -> Intent {
    let id = format!(
        "clear-veto-{instrument}-{name}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::ClearVeto, instrument, id, now);
    intent.name = Some(name.to_string());
    intent
}

/// Build the signed wire body for control-path intents (status,
/// unlock, prep, veto, clear-prep, clear-veto). The intent fields go at
/// the top level next to the shell fields, and a `sig:` line is appended
/// at the bottom. The shell carries concrete zeros + `now` — TradingView
/// isn't in this loop. See `core::sig` for the canonical form.
pub fn wrap_signed(intent: &Intent, key: &[u8; KEY_LEN], now: DateTime<Utc>) -> Result<String> {
    build_signed_body(intent, key, &shell_for_control(now))
}

/// Build a signed body for a TradingView alert. Shell fields are the
/// literal TradingView placeholders so the alert template substitutes
/// them at delivery time without invalidating the sig.
pub fn wrap_signed_template(intent: &Intent, key: &[u8; KEY_LEN]) -> Result<String> {
    build_signed_body(intent, key, &shell_for_tv_template())
}

fn build_signed_body(
    intent: &Intent,
    key: &[u8; KEY_LEN],
    shell_lines: &[(&str, String)],
) -> Result<String> {
    // Serialise the intent and re-parse so each field becomes a YAML
    // value. Then emit each top-level field on a single line, where
    // nested values are flow-style (`{type: market}`). Both the CLI
    // emit step and the worker verify step line-scan the same text.
    let intent_yaml = serde_yaml::to_string(intent).map_err(|e| eyre!("serialise intent: {e}"))?;
    let intent_value: serde_yaml::Value =
        serde_yaml::from_str(&intent_yaml).map_err(|e| eyre!("re-parse intent: {e}"))?;
    let intent_map = intent_value
        .as_mapping()
        .ok_or_else(|| eyre!("intent did not serialise to a mapping"))?;

    let mut lines: Vec<String> = Vec::new();
    for (k, v) in shell_lines {
        lines.push(format!("{k}: {v}"));
    }
    for (k, v) in intent_map {
        let key_str = k.as_str().ok_or_else(|| eyre!("non-string intent key"))?;
        let val_str = render_value(v)?;
        lines.push(format!("{key_str}: {val_str}"));
    }
    // Build the body without the sig line, then line-scan it the same
    // way the worker will. This guarantees the canonical pair list is
    // exactly what the verify side will reconstruct.
    let body_without_sig = format!("{}\n", lines.join("\n"));
    let pairs = trade_control_core::incoming::signed_pairs_from_text(&body_without_sig)
        .map_err(|e| eyre!("build signed pairs: {e}"))?;
    let sig = trade_control_core::sig::sign(key, &pairs).map_err(|e| eyre!("sign: {e}"))?;
    Ok(format!("{body_without_sig}sig: \"{sig}\"\n"))
}

fn shell_for_control(now: DateTime<Utc>) -> Vec<(&'static str, String)> {
    vec![
        ("close", "0".to_string()),
        ("high", "0".to_string()),
        ("low", "0".to_string()),
        ("time", format!("\"{}\"", now.to_rfc3339())),
    ]
}

fn shell_for_tv_template() -> Vec<(&'static str, String)> {
    vec![
        ("close", "{{close}}".to_string()),
        ("high", "{{high}}".to_string()),
        ("low", "{{low}}".to_string()),
        ("time", "\"{{time}}\"".to_string()),
        // signal_* / golden / atr come from candle-signals-v2.pine's
        // hidden plots, populated when the Long/Short Pattern
        // alertcondition fires. The worker treats them as optional —
        // pre-2026-05 v2 signed templates parse unchanged. Note:
        // {{plot("…")}} names the Pine plot title; the YAML key on
        // the left is the wire-key the worker deserialises into the
        // Shell. They don't have to match — `golden` / `atr` live
        // under `signal_golden` / `signal_atr` Pine titles.
        ("signal_high", "{{plot(\"signal_high\")}}".to_string()),
        ("signal_low", "{{plot(\"signal_low\")}}".to_string()),
        ("signal_range", "{{plot(\"signal_range\")}}".to_string()),
        (
            "signal_start_time",
            "{{plot(\"signal_start_time\")}}".to_string(),
        ),
        ("signal_kind", "{{plot(\"signal_kind\")}}".to_string()),
        ("golden", "{{plot(\"signal_golden\")}}".to_string()),
        ("atr", "{{plot(\"signal_atr\")}}".to_string()),
        (
            "signal_confirmed",
            "{{plot(\"signal_confirmed\")}}".to_string(),
        ),
    ]
}

/// Render a YAML value as a single-line string, suitable for going on
/// the right side of a top-level `key: value` line. Scalars use their
/// raw form; nested values use serde_yaml's flow-style serialisation.
fn render_value(v: &serde_yaml::Value) -> Result<String> {
    match v {
        serde_yaml::Value::Null => Ok("~".to_string()),
        serde_yaml::Value::Bool(b) => Ok(b.to_string()),
        serde_yaml::Value::Number(n) => Ok(n.to_string()),
        serde_yaml::Value::String(s) => {
            // Quote strings that might confuse the line-scan parser
            // (timestamps with colons, etc.). Quoting is safe for any
            // string — the worker strips matching quotes.
            if needs_quoting(s) {
                Ok(format!("\"{}\"", s.replace('"', "\\\"")))
            } else {
                Ok(s.clone())
            }
        }
        _ => {
            // serde_yaml emits block-style by default. To force a single
            // line for nested structures we serialise via JSON (which is
            // a valid subset of flow YAML).
            let json = serde_json::to_string(v).map_err(|e| eyre!("flow-style serialise: {e}"))?;
            Ok(json)
        }
    }
}

fn needs_quoting(s: &str) -> bool {
    // Quote anything containing characters that aren't safe in a plain
    // YAML scalar on a single line: colons (timestamps), leading/trailing
    // whitespace, or starts that YAML would parse as a special type.
    if s.is_empty() {
        return true;
    }
    if s.chars()
        .any(|c| matches!(c, ':' | '#' | '\'' | '"' | '\n'))
    {
        return true;
    }
    if s.starts_with(|c: char| c.is_whitespace()) || s.ends_with(|c: char| c.is_whitespace()) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> DateTime<Utc> {
        "2026-05-14T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn status_intent_has_status_action() {
        let intent = build_status_intent(t(), "ab12");
        assert_eq!(intent.action, Action::Status);
        assert_eq!(intent.instrument, "ALL");
        assert_eq!(intent.not_after, t() + CONTROL_TTL);
        assert!(intent.id.starts_with("status-"));
        assert!(intent.id.ends_with("-ab12"));
    }

    #[test]
    fn unlock_intent_carries_instrument() {
        let intent = build_unlock_intent("EUR_USD", t(), "cd34");
        assert_eq!(intent.action, Action::Unlock);
        assert_eq!(intent.instrument, "EUR_USD");
        assert_eq!(intent.not_after, t() + CONTROL_TTL);
        assert!(intent.id.starts_with("unlock-EUR_USD-"));
        assert!(intent.id.ends_with("-cd34"));
    }

    #[test]
    fn intent_round_trips_through_yaml() {
        // The Intent we build must deserialise back through serde_yaml so
        // the worker's parser is happy with it end-to-end.
        let intent = build_unlock_intent("USD_JPY", t(), "x1");
        let yaml = serde_yaml::to_string(&intent).unwrap();
        let parsed: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.action, Action::Unlock);
        assert_eq!(parsed.instrument, "USD_JPY");
    }

    #[test]
    fn prep_intent_round_trips() {
        let intent = build_prep_intent("EUR_USD", "break-and-close", 4, Vec::new(), t(), "ab12");
        assert_eq!(intent.action, Action::Prep);
        assert_eq!(intent.instrument, "EUR_USD");
        assert_eq!(intent.step.as_deref(), Some("break-and-close"));
        assert_eq!(intent.ttl_hours, Some(4));
        assert!(intent.clears.is_empty());
        let yaml = serde_yaml::to_string(&intent).unwrap();
        let parsed: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.action, Action::Prep);
        assert_eq!(parsed.step.as_deref(), Some("break-and-close"));
        assert!(parsed.clears.is_empty());
    }

    #[test]
    fn prep_intent_carries_clears_through_yaml() {
        let intent = build_prep_intent(
            "EUR_USD",
            "break-and-close",
            4,
            vec!["retest".into()],
            t(),
            "ab12",
        );
        assert_eq!(intent.clears, vec!["retest".to_string()]);
        let yaml = serde_yaml::to_string(&intent).unwrap();
        assert!(yaml.contains("clears:"), "yaml was:\n{yaml}");
        let parsed: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.clears, vec!["retest".to_string()]);
    }

    #[test]
    fn veto_intent_round_trips() {
        let intent = build_veto_intent("EUR_USD", "news-window", 6, None, Vec::new(), t(), "cd34");
        assert_eq!(intent.action, Action::Veto);
        assert_eq!(intent.name.as_deref(), Some("news-window"));
        assert_eq!(intent.ttl_hours, Some(6));
        assert!(intent.clears.is_empty());
        let yaml = serde_yaml::to_string(&intent).unwrap();
        let parsed: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.action, Action::Veto);
    }

    #[test]
    fn clear_prep_intent_carries_step() {
        let intent = build_clear_prep_intent("EUR_USD", "retest", t(), "ef56");
        assert_eq!(intent.action, Action::ClearPrep);
        assert_eq!(intent.step.as_deref(), Some("retest"));
        assert_eq!(intent.ttl_hours, None);
    }

    #[test]
    fn clear_veto_intent_carries_name() {
        let intent = build_clear_veto_intent("EUR_USD", "news-window", t(), "gh78");
        assert_eq!(intent.action, Action::ClearVeto);
        assert_eq!(intent.name.as_deref(), Some("news-window"));
    }

    #[test]
    fn signed_control_body_contains_concrete_shell_and_sig() {
        let key = [0u8; KEY_LEN];
        let intent = build_status_intent(t(), "ab12");
        let body = wrap_signed(&intent, &key, t()).unwrap();
        // Shell must be concrete (no TradingView placeholders).
        assert!(body.contains("close: 0"), "body was:\n{body}");
        assert!(body.contains("high: 0"), "body was:\n{body}");
        assert!(body.contains("low: 0"), "body was:\n{body}");
        assert!(body.contains("time:"), "body was:\n{body}");
        assert!(!body.contains("{{close}}"), "body was:\n{body}");
        assert!(body.contains("sig: \"v1-sig."), "body was:\n{body}");
    }
}
