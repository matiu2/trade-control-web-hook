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
        risk_pct: trade_control_core::tunable::Tunable::Static(1.0),
        risk_amount: None,
        size_units: None,
        dry_run: None,
        cooldown_hours: None,
        min_r: None,
        broker: BrokerKind::Oanda,
        step: None,
        name: None,
        ttl_hours: trade_control_core::tunable::Tunable::Static(0),
        level: None,
        requires_preps: Vec::new(),
        vetos: Vec::new(),
        clears: Vec::new(),
        account: None,
        trade_id: None,
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
    intent.ttl_hours = trade_control_core::tunable::Tunable::Static(ttl_hours);
    intent.clears = clears;
    intent
}

/// Build a `veto` Intent for a single (trade_id, instrument, name) with a
/// TTL. `level` controls broker side effects at fire time; pass `None` for
/// the default flag-only behaviour ([`VetoLevel::StopNextEntry`]). `clears`
/// lists other vetos to drop before recording this one. `trade_id` scopes
/// the veto to one setup so it can't bleed into another on the same
/// instrument — the worker rejects a veto without one.
#[allow(clippy::too_many_arguments)]
pub fn build_veto_intent(
    instrument: &str,
    trade_id: &str,
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
    intent.trade_id = Some(trade_id.to_string());
    intent.ttl_hours = trade_control_core::tunable::Tunable::Static(ttl_hours);
    intent.level = level;
    intent.clears = clears;
    intent
}

/// Build a `clear-prep` Intent for a single (instrument, step) pair.
///
/// `account` scopes the clear: `None` targets the global (`_`) prep,
/// `Some("reversals")` an account-scoped one. It must match the scope the
/// prep was set under (the worker keys preps by `(account, instrument,
/// step)`), else the clear is a silent no-op.
pub fn build_clear_prep_intent(
    instrument: &str,
    step: &str,
    account: Option<&str>,
    now: DateTime<Utc>,
    suffix: &str,
) -> Intent {
    let id = format!(
        "clear-prep-{instrument}-{step}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::ClearPrep, instrument, id, now);
    intent.step = Some(step.to_string());
    intent.account = account.map(str::to_string);
    intent
}

/// Build a `clear-veto` Intent for a single (instrument, name) pair.
///
/// `account` scopes the clear the same way as [`build_clear_prep_intent`]
/// (the worker keys vetos by `(account, trade_id, instrument, name)`).
pub fn build_clear_veto_intent(
    instrument: &str,
    trade_id: &str,
    name: &str,
    account: Option<&str>,
    now: DateTime<Utc>,
    suffix: &str,
) -> Intent {
    let id = format!(
        "clear-veto-{instrument}-{name}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::ClearVeto, instrument, id, now);
    intent.name = Some(name.to_string());
    intent.trade_id = Some(trade_id.to_string());
    intent.account = account.map(str::to_string);
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

/// Build a signed body for a TradingView alert that fires from a Pine
/// study (e.g. `Candle Signals`'s `Long/Short Pattern` alertcondition).
/// Shell includes the `{{plot("…")}}` placeholders that only resolve
/// when the alert is bound to a study — see [`shell_for_tv_template_pine`].
///
/// When the intent opts into bar-based order expiry (`expiry_bars`
/// set), the shell additionally carries the
/// `next_candle_timestamp_1..5` menu placeholders. They're appended
/// only in that case so trades that don't use the feature stay
/// byte-identical on the wire and don't depend on an indicator that
/// ships the menu plots — an operator who sets `expiry_bars` is
/// asserting they're on the v2+ indicator that does.
pub fn wrap_signed_template(intent: &Intent, key: &[u8; KEY_LEN]) -> Result<String> {
    let mut shell = shell_for_tv_template_pine();
    if intent.expiry_bars.is_some() {
        shell.extend(shell_for_next_candle_menu());
    }
    build_signed_body(intent, key, &shell)
}

/// Build a signed body for a TradingView alert that fires from a
/// drawing (horizontal line, vertical line, trendline). Drawings have
/// no Pine context, so `{{plot("…")}}` placeholders would be delivered
/// literally and crash the worker's YAML parser. Only the four
/// universally-substituted placeholders (`close`/`high`/`low`/`time`)
/// are included; signal-bar fields are simply absent and the worker's
/// optional `Shell` fields stay `None`.
pub fn wrap_signed_template_drawing(intent: &Intent, key: &[u8; KEY_LEN]) -> Result<String> {
    build_signed_body(intent, key, &shell_for_tv_template_drawing())
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

fn shell_for_tv_template_pine() -> Vec<(&'static str, String)> {
    let mut lines = shell_for_tv_template_drawing();
    // signal_* / golden / atr come from candle-signals-v2.pine's
    // hidden plots, populated when the Long/Short Pattern
    // alertcondition fires. The worker treats them as optional —
    // pre-2026-05 v2 signed templates parse unchanged. Note:
    // {{plot("…")}} names the Pine plot title; the YAML key on
    // the left is the wire-key the worker deserialises into the
    // Shell. They don't have to match — `golden` / `atr` live
    // under `signal_golden` / `signal_atr` Pine titles.
    lines.extend([
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
        ("recent_high", "{{plot(\"recent_high\")}}".to_string()),
        ("recent_low", "{{plot(\"recent_low\")}}".to_string()),
    ]);
    lines
}

/// The five forward bar-close menu placeholders, filled by Pine at
/// fire-time via `time_close(timeframe.period, bars_back=-k)`. Appended
/// to the Pine-bound enter shell only when `expiry_bars` is set. The
/// worker indexes this menu with the signed `expiry_bars` to derive the
/// order's `cancel_at`. Like the `signal_*` plots, the YAML key on the
/// left is what the worker deserialises; the `{{plot("…")}}` names the
/// Pine plot title.
fn shell_for_next_candle_menu() -> Vec<(&'static str, String)> {
    (1..=5)
        .map(|n| {
            let key: &'static str = match n {
                1 => "next_candle_timestamp_1",
                2 => "next_candle_timestamp_2",
                3 => "next_candle_timestamp_3",
                4 => "next_candle_timestamp_4",
                _ => "next_candle_timestamp_5",
            };
            (key, format!("{{{{plot(\"next_candle_timestamp_{n}\")}}}}"))
        })
        .collect()
}

fn shell_for_tv_template_drawing() -> Vec<(&'static str, String)> {
    vec![
        ("close", "{{close}}".to_string()),
        ("high", "{{high}}".to_string()),
        ("low", "{{low}}".to_string()),
        // `open` is a TradingView built-in (no plot-index risk). It rides
        // every TV-template shell so the M/W enter can compute candle-body
        // extremes (rogue-wick handling, dynamic neckline revision). Its
        // value is unsigned (TV fills it post-sign); see
        // `trade_control_core::sig::UNSIGNED_VALUE_KEYS`. Optional on the
        // worker side, so older charts without it still verify.
        ("open", "{{open}}".to_string()),
        ("time", "\"{{time}}\"".to_string()),
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
        match &intent.ttl_hours {
            trade_control_core::tunable::Tunable::Static(n) => assert_eq!(*n, 4),
            other => panic!("expected Static(4) ttl_hours, got {other:?}"),
        }
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
        let intent = build_veto_intent(
            "EUR_USD",
            "eurusd-hs-1",
            "news-window",
            6,
            None,
            Vec::new(),
            t(),
            "cd34",
        );
        assert_eq!(intent.action, Action::Veto);
        assert_eq!(intent.name.as_deref(), Some("news-window"));
        assert_eq!(intent.trade_id.as_deref(), Some("eurusd-hs-1"));
        match &intent.ttl_hours {
            trade_control_core::tunable::Tunable::Static(n) => assert_eq!(*n, 6),
            other => panic!("expected Static(6) ttl_hours, got {other:?}"),
        }
        assert!(intent.clears.is_empty());
        let yaml = serde_yaml::to_string(&intent).unwrap();
        let parsed: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.action, Action::Veto);
        // The built intent must pass worker-side validation (trade_id
        // is now mandatory on veto).
        intent.validate().expect("veto intent should validate");
    }

    #[test]
    fn clear_prep_intent_carries_step() {
        // Default (global) scope: no account.
        let intent = build_clear_prep_intent("EUR_USD", "retest", None, t(), "ef56");
        assert_eq!(intent.action, Action::ClearPrep);
        assert_eq!(intent.step.as_deref(), Some("retest"));
        assert_eq!(intent.account, None);
        assert!(matches!(
            intent.ttl_hours,
            trade_control_core::tunable::Tunable::Static(0)
        ));
    }

    #[test]
    fn clear_prep_intent_carries_account_scope() {
        let intent = build_clear_prep_intent("EUR_USD", "retest", Some("reversals"), t(), "ef56");
        assert_eq!(intent.account.as_deref(), Some("reversals"));
    }

    #[test]
    fn clear_veto_intent_carries_name() {
        let intent =
            build_clear_veto_intent("EUR_USD", "eurusd-hs-1", "news-window", None, t(), "gh78");
        assert_eq!(intent.action, Action::ClearVeto);
        assert_eq!(intent.name.as_deref(), Some("news-window"));
        assert_eq!(intent.trade_id.as_deref(), Some("eurusd-hs-1"));
        intent
            .validate()
            .expect("clear-veto intent should validate");
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
