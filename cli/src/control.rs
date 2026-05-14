//! Helpers for building `status` / `unlock` control envelopes from CLI args.
//!
//! TradingView is not in the loop for these — the CLI POSTs them directly.
//! The shell fields are still required by the worker's parser, so we fill
//! them with concrete zero values plus a real timestamp.

use chrono::{DateTime, Duration, Utc};
use color_eyre::eyre::{Result, eyre};

use crate::{build_yaml_control_body, encrypt_intent};
use trade_control_core::crypto::KEY_LEN;
use trade_control_core::intent::{Action, BrokerKind, Intent};

/// How long a control envelope stays valid. Short — these are one-shot
/// commands run by hand, so we don't need a long replay window.
const CONTROL_TTL: Duration = Duration::minutes(5);

/// Placeholder instrument string used in `status` envelopes. The action
/// ignores the field, but `ALWAYS_REQUIRED` insists it be present.
const STATUS_INSTRUMENT: &str = "ALL";

/// Build a status `Intent`. `suffix` is a short random tag appended to the
/// id so two concurrent status calls don't collide on replay protection.
pub fn build_status_intent(now: DateTime<Utc>, suffix: &str) -> Intent {
    Intent {
        v: 1,
        id: format!("status-{}-{suffix}", now.format("%Y-%m-%dT%H%M%S")),
        not_before: None,
        not_after: now + CONTROL_TTL,
        action: Action::Status,
        instrument: STATUS_INSTRUMENT.to_string(),
        direction: None,
        entry: None,
        stop_loss: None,
        take_profit: None,
        risk_pct: None,
        cooldown_hours: None,
        min_r: None,
        broker: BrokerKind::Oanda,
    }
}

/// Build an unlock `Intent` for a single instrument.
pub fn build_unlock_intent(instrument: &str, now: DateTime<Utc>, suffix: &str) -> Intent {
    Intent {
        v: 1,
        id: format!(
            "unlock-{instrument}-{}-{suffix}",
            now.format("%Y-%m-%dT%H%M%S")
        ),
        not_before: None,
        not_after: now + CONTROL_TTL,
        action: Action::Unlock,
        instrument: instrument.to_string(),
        direction: None,
        entry: None,
        stop_loss: None,
        take_profit: None,
        risk_pct: None,
        cooldown_hours: None,
        min_r: None,
        broker: BrokerKind::Oanda,
    }
}

/// Serialise the intent as YAML, encrypt under `key`, and wrap in the
/// hybrid plaintext-shell envelope the worker expects.
pub fn wrap_in_envelope(
    intent: &Intent,
    key: &[u8; KEY_LEN],
    now: DateTime<Utc>,
) -> Result<String> {
    let plaintext = serde_yaml::to_string(intent).map_err(|e| eyre!("serialise intent: {e}"))?;
    let blob = encrypt_intent(key, plaintext.as_bytes())?;
    Ok(build_yaml_control_body(&blob, now))
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
    fn envelope_contains_concrete_shell_and_payload() {
        let key = [0u8; KEY_LEN];
        let intent = build_status_intent(t(), "ab12");
        let body = wrap_in_envelope(&intent, &key, t()).unwrap();
        // Shell must be concrete (no TradingView placeholders).
        assert!(body.contains("close: 0"), "body was:\n{body}");
        assert!(body.contains("high: 0"), "body was:\n{body}");
        assert!(body.contains("low: 0"), "body was:\n{body}");
        assert!(body.contains("time:"), "body was:\n{body}");
        assert!(!body.contains("{{close}}"), "body was:\n{body}");
        assert!(body.contains("payload: \"v1."), "body was:\n{body}");
    }
}
