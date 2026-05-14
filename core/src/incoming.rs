//! Hybrid YAML payload parsing.
//!
//! The body has two parts in a single YAML doc:
//!   - plaintext shell: `close`, `high`, `low`, `time`, `payload`  (TradingView fills these)
//!   - encrypted blob: `payload: "v1.<base64>"`  (decrypts to a YAML `Intent`)
//!
//! This module parses + decrypts + sanity-checks. Replay protection and
//! cooldown checks live in the dispatch layer because they need a `StateStore`.

use chrono::{DateTime, Utc};

use crate::crypto::{self, CryptoError};
use crate::intent::{Intent, Shell};

#[derive(Debug)]
pub enum IncomingError {
    BadYaml,
    Decrypt(CryptoError),
    BadIntentYaml,
    UnsupportedVersion(u32),
    /// `time` (from TradingView) is too far from `now`.
    StaleShellTime,
    /// `now < not_before`.
    TooEarly,
    /// `now > not_after`.
    Expired,
    /// `instrument` in URL/route differs from the encrypted intent's instrument.
    /// We don't currently route by URL, but check that the plaintext payload
    /// hasn't been swapped onto a different market via prefix games. Currently
    /// not used; kept for future hardening.
    #[allow(dead_code)]
    InstrumentMismatch,
}

impl core::fmt::Display for IncomingError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadYaml => f.write_str("invalid YAML"),
            Self::Decrypt(e) => write!(f, "decrypt: {e}"),
            Self::BadIntentYaml => f.write_str("decrypted intent is not valid YAML"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported intent version {v}"),
            Self::StaleShellTime => f.write_str("plaintext time too far from now"),
            Self::TooEarly => f.write_str("intent not yet valid"),
            Self::Expired => f.write_str("intent expired"),
            Self::InstrumentMismatch => f.write_str("instrument mismatch"),
        }
    }
}

impl std::error::Error for IncomingError {}

/// How far either side of `now` the plaintext `time` is allowed to be. A wide
/// window stays robust to clock drift but tight enough to catch ancient replays.
const MAX_SHELL_SKEW_HOURS: i64 = 24;

/// Parsed + decrypted + time-sanity-checked package, ready for the dispatch
/// layer to run replay / cooldown / risk gates.
#[derive(Debug, Clone)]
pub struct Verified {
    pub shell: Shell,
    pub intent: Intent,
}

/// Parse YAML body, decrypt the payload, sanity-check timestamps. Does NOT
/// check replay or cooldown — those need a `StateStore`.
pub fn parse_and_verify(
    yaml: &str,
    key: &[u8],
    now: DateTime<Utc>,
) -> Result<Verified, IncomingError> {
    let shell: Shell = serde_yaml::from_str(yaml).map_err(|_| IncomingError::BadYaml)?;

    let plaintext = crypto::decrypt(key, &shell.payload).map_err(IncomingError::Decrypt)?;
    let intent: Intent =
        serde_yaml::from_slice(&plaintext).map_err(|_| IncomingError::BadIntentYaml)?;

    if intent.v != 1 {
        return Err(IncomingError::UnsupportedVersion(intent.v));
    }

    let skew = (shell.time - now).num_hours().abs();
    if skew > MAX_SHELL_SKEW_HOURS {
        return Err(IncomingError::StaleShellTime);
    }

    if let Some(nbf) = intent.not_before
        && now < nbf
    {
        return Err(IncomingError::TooEarly);
    }
    if now > intent.not_after {
        return Err(IncomingError::Expired);
    }

    Ok(Verified { shell, intent })
}

/// How long to remember a fulfilled `id` for replay protection.
/// Caller passes (not_after - now); we clamp to a sane minimum.
pub fn replay_ttl_seconds(not_after: DateTime<Utc>, now: DateTime<Utc>) -> u64 {
    let delta = not_after - now;
    let secs = delta.num_seconds().max(0) as u64;
    // Add 1 hour of grace so the seen-key outlives the not_after window itself.
    secs.saturating_add(3600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::encrypt_with_nonce;
    use chrono::Duration;

    const KEY: [u8; 32] = [9u8; 32];
    const NONCE: [u8; 12] = [4u8; 12];

    fn intent_yaml(not_after: &str) -> String {
        format!(
            "v: 1\n\
             id: abc\n\
             not_after: \"{not_after}\"\n\
             action: enter\n\
             instrument: EUR_USD\n\
             direction: long\n\
             entry: {{ type: market }}\n\
             stop_loss: {{ from: low, offset_pips: -2 }}\n\
             take_profit: {{ from: close, offset_r: 2.0 }}\n\
             risk_pct: 0.5\n"
        )
    }

    fn build_yaml(intent_yaml: &str, time: &str) -> String {
        let blob = encrypt_with_nonce(&KEY, &NONCE, intent_yaml.as_bytes()).unwrap();
        format!("close: 1.1000\nhigh: 1.1020\nlow: 1.0980\ntime: \"{time}\"\npayload: \"{blob}\"\n")
    }

    #[test]
    fn happy_path_parses_and_decrypts() {
        let yaml = build_yaml(&intent_yaml("2026-05-13T20:00:00Z"), "2026-05-13T12:00:00Z");
        let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
        let v = parse_and_verify(&yaml, &KEY, now).unwrap();
        assert_eq!(v.intent.id, "abc");
        assert!((v.shell.close - 1.1000).abs() < 1e-9);
    }

    #[test]
    fn wrong_key_rejected() {
        let yaml = build_yaml(&intent_yaml("2026-05-13T20:00:00Z"), "2026-05-13T12:00:00Z");
        let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
        let bad_key = [1u8; 32];
        assert!(matches!(
            parse_and_verify(&yaml, &bad_key, now),
            Err(IncomingError::Decrypt(_))
        ));
    }

    #[test]
    fn stale_shell_time_rejected() {
        // shell time is 2 days before now
        let yaml = build_yaml(&intent_yaml("2030-01-01T00:00:00Z"), "2026-05-13T00:00:00Z");
        let now: DateTime<Utc> = "2026-05-15T00:00:01Z".parse().unwrap();
        assert!(matches!(
            parse_and_verify(&yaml, &KEY, now),
            Err(IncomingError::StaleShellTime)
        ));
    }

    #[test]
    fn expired_rejected() {
        let yaml = build_yaml(&intent_yaml("2026-05-13T12:00:00Z"), "2026-05-13T11:59:00Z");
        let now: DateTime<Utc> = "2026-05-13T13:00:00Z".parse().unwrap();
        assert!(matches!(
            parse_and_verify(&yaml, &KEY, now),
            Err(IncomingError::Expired)
        ));
    }

    #[test]
    fn too_early_rejected() {
        let intent = "v: 1\n\
             id: abc\n\
             not_before: \"2026-05-13T13:00:00Z\"\n\
             not_after: \"2026-05-13T20:00:00Z\"\n\
             action: enter\n\
             instrument: EUR_USD\n\
             direction: long\n\
             entry: { type: market }\n\
             stop_loss: { from: low, offset_pips: -2 }\n\
             take_profit: { from: close, offset_r: 2.0 }\n\
             risk_pct: 0.5\n";
        let yaml = build_yaml(intent, "2026-05-13T12:00:00Z");
        let now: DateTime<Utc> = "2026-05-13T12:00:00Z".parse().unwrap();
        assert!(matches!(
            parse_and_verify(&yaml, &KEY, now),
            Err(IncomingError::TooEarly)
        ));
    }

    #[test]
    fn unsupported_version_rejected() {
        let intent = "v: 99\n\
             id: abc\n\
             not_after: \"2030-01-01T00:00:00Z\"\n\
             action: close\n\
             instrument: EUR_USD\n";
        let yaml = build_yaml(intent, "2026-05-13T12:00:00Z");
        let now: DateTime<Utc> = "2026-05-13T12:00:00Z".parse().unwrap();
        assert!(matches!(
            parse_and_verify(&yaml, &KEY, now),
            Err(IncomingError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn replay_ttl_has_grace() {
        let now: DateTime<Utc> = "2026-05-13T12:00:00Z".parse().unwrap();
        let not_after = now + Duration::hours(2);
        let ttl = replay_ttl_seconds(not_after, now);
        // 2 hours + 1 hour grace = 3h = 10800s
        assert_eq!(ttl, 10_800);
    }

    #[test]
    fn replay_ttl_for_already_expired_is_grace() {
        let now: DateTime<Utc> = "2026-05-13T12:00:00Z".parse().unwrap();
        let not_after = now - Duration::hours(5);
        let ttl = replay_ttl_seconds(not_after, now);
        assert_eq!(ttl, 3600);
    }
}
