//! Hybrid YAML payload parsing.
//!
//! Two wire formats are accepted:
//!   1. **Encrypted** — flat YAML with `close`, `high`, `low`, `time`,
//!      `payload: "v1.<base64>"`. The payload decrypts to an inner YAML
//!      `Intent`. Original format; kept for back-compat.
//!   2. **Signed** — flat YAML where the intent fields are at the top
//!      level alongside the shell, plus a `sig: "v1-sig.<base64>"` field.
//!      The cleartext shows up in Cloudflare's request log, which makes
//!      operator debugging much easier. See [`crate::sig`] for the
//!      canonical form.
//!
//! Detection is by field presence: a `sig:` field selects the signed
//! path, otherwise we try the encrypted path.
//!
//! This module parses + decrypts/verifies + sanity-checks timestamps.
//! Replay protection and cooldown checks live in the dispatch layer
//! because they need a `StateStore`.

use chrono::{DateTime, Utc};

use crate::crypto::{self, CryptoError};
use crate::intent::{Intent, Shell};
use crate::sig::{self, SIG_FIELD, SigError};

#[derive(Debug)]
pub enum IncomingError {
    BadYaml,
    Decrypt(CryptoError),
    /// Sig verification failed on the signed path.
    Sig(SigError),
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
            Self::Sig(e) => write!(f, "sig: {e}"),
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

/// Parse + verify a body in either the encrypted or signed wire format.
/// Detection is by field presence: a top-level `sig:` selects signing,
/// otherwise the encrypted path runs. Both paths converge on the same
/// timestamp / version checks.
pub fn parse_and_verify(
    yaml: &str,
    key: &[u8],
    now: DateTime<Utc>,
) -> Result<Verified, IncomingError> {
    if has_sig_field(yaml) {
        parse_and_verify_signed(yaml, key, now)
    } else {
        parse_and_verify_encrypted(yaml, key, now)
    }
}

fn has_sig_field(yaml: &str) -> bool {
    // Use a top-level YAML mapping check rather than line scan so a
    // `sig:` substring inside a nested value can't trick us.
    let Ok(serde_yaml::Value::Mapping(map)) = serde_yaml::from_str::<serde_yaml::Value>(yaml)
    else {
        return false;
    };
    map.contains_key(serde_yaml::Value::String(SIG_FIELD.to_string()))
}

fn parse_and_verify_encrypted(
    yaml: &str,
    key: &[u8],
    now: DateTime<Utc>,
) -> Result<Verified, IncomingError> {
    let shell: Shell = serde_yaml::from_str(yaml).map_err(|_| IncomingError::BadYaml)?;
    let plaintext = crypto::decrypt(key, &shell.payload).map_err(IncomingError::Decrypt)?;
    let intent: Intent =
        serde_yaml::from_slice(&plaintext).map_err(|_| IncomingError::BadIntentYaml)?;
    check_intent_freshness(&shell, &intent, now)?;
    Ok(Verified { shell, intent })
}

/// Parse a signed body.
///
/// The signing input is built by **line scanning** the raw body text —
/// every line of the form `<key>: <value>` at the top level (no
/// indentation) becomes a `(key, value_raw)` pair. We sign the raw
/// textual value to avoid number-format / quoting drift between the
/// CLI's emit step and the worker's verify step.
///
/// Constraints on the signed wire format:
///   - One field per line at indent 0.
///   - Multi-field structures (e.g. enter's `entry`) serialise as inline
///     flow-style YAML on a single line: `entry: {type: market}`.
///   - `sig` MUST be the last line (or anywhere — we exclude it from
///     signing regardless).
fn parse_and_verify_signed(
    yaml: &str,
    key: &[u8],
    now: DateTime<Utc>,
) -> Result<Verified, IncomingError> {
    let pairs = signed_pairs_from_text(yaml)?;
    let sig_str = pairs_get(&pairs, SIG_FIELD).ok_or(IncomingError::Sig(SigError::MissingSig))?;
    let without_sig: Vec<_> = pairs
        .iter()
        .filter(|(k, _)| k != SIG_FIELD)
        .cloned()
        .collect();
    sig::verify(key, &without_sig, sig_str).map_err(IncomingError::Sig)?;

    // After verification, parse the YAML normally to get strong-typed
    // Shell and Intent. Drop the `sig` field for Intent deserialisation
    // because the Intent struct doesn't know about it.
    let value: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|_| IncomingError::BadYaml)?;
    let mapping = value.as_mapping().ok_or(IncomingError::BadYaml)?;

    let mut intent_map = serde_yaml::Mapping::new();
    for (k, v) in mapping {
        let key_str = k.as_str().unwrap_or("");
        if matches!(key_str, "close" | "high" | "low" | "time" | "sig") {
            continue;
        }
        intent_map.insert(k.clone(), v.clone());
    }
    let intent: Intent = serde_yaml::from_value(serde_yaml::Value::Mapping(intent_map))
        .map_err(|_| IncomingError::BadIntentYaml)?;
    // Shell wants `payload` (encrypted-path), so build it field-by-field
    // for the signed path. The `payload` field is unused once we've
    // verified, so fill a placeholder.
    let shell = Shell {
        close: mapping_f64(mapping, "close")?,
        high: mapping_f64(mapping, "high")?,
        low: mapping_f64(mapping, "low")?,
        time: mapping_time(mapping, "time")?,
        payload: String::new(),
    };

    check_intent_freshness(&shell, &intent, now)?;
    Ok(Verified { shell, intent })
}

fn pairs_get<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Pull `(key, value)` pairs out of the raw body text by line scan.
///
/// Strips surrounding quotes from the value so `sig: "v1-sig.xxx"`
/// yields `("sig", "v1-sig.xxx")` and `time: "{{time}}"` yields
/// `("time", "{{time}}")`. This is the *exact same view* the CLI's
/// sign step takes — no YAML round-trip — so the signed input is the
/// raw bytes either side sees.
///
/// Public so the CLI can call it in lockstep with the worker.
pub fn signed_pairs_from_text(yaml: &str) -> Result<Vec<(String, String)>, IncomingError> {
    let mut out = Vec::new();
    for line in yaml.lines() {
        // Top-level pairs have no leading whitespace.
        if line.is_empty() || line.starts_with(char::is_whitespace) || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim().to_string();
        if key.is_empty() {
            return Err(IncomingError::BadYaml);
        }
        let val = strip_yaml_quotes(v.trim()).to_string();
        out.push((key, val));
    }
    Ok(out)
}

fn mapping_f64(map: &serde_yaml::Mapping, key: &str) -> Result<f64, IncomingError> {
    map.get(serde_yaml::Value::String(key.to_string()))
        .and_then(|v| v.as_f64())
        .ok_or(IncomingError::BadYaml)
}

fn mapping_time(map: &serde_yaml::Mapping, key: &str) -> Result<DateTime<Utc>, IncomingError> {
    let s = map
        .get(serde_yaml::Value::String(key.to_string()))
        .and_then(|v| v.as_str())
        .ok_or(IncomingError::BadYaml)?;
    s.parse::<DateTime<Utc>>()
        .map_err(|_| IncomingError::BadYaml)
}

fn strip_yaml_quotes(s: &str) -> &str {
    if let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return inner;
    }
    if let Some(inner) = s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return inner;
    }
    s
}

fn check_intent_freshness(
    shell: &Shell,
    intent: &Intent,
    now: DateTime<Utc>,
) -> Result<(), IncomingError> {
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
    Ok(())
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

    // ===== Signed-path tests =====

    /// Build a signed body for a `prep` intent. Mirrors what the CLI's
    /// sign step will emit: shell fields on top, intent fields next,
    /// `sig` at the bottom.
    fn build_signed_prep(not_after: &str, time: &str) -> String {
        let body_without_sig = [
            "close: 1.1000",
            "high: 1.1020",
            "low: 1.0980",
            &format!("time: \"{time}\""),
            "v: 1",
            "action: prep",
            "instrument: EUR_USD",
            "id: prep-abc",
            &format!("not_after: \"{not_after}\""),
            "step: retest",
            "ttl_hours: 12",
            "",
        ]
        .join("\n");
        let pairs = signed_pairs_from_text(&body_without_sig).unwrap();
        let sig = crate::sig::sign(&KEY, &pairs).unwrap();
        format!("{body_without_sig}sig: \"{sig}\"\n")
    }

    #[test]
    fn signed_path_happy() {
        let yaml = build_signed_prep("2026-05-13T20:00:00Z", "2026-05-13T12:00:00Z");
        let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
        let v = parse_and_verify(&yaml, &KEY, now).unwrap();
        assert_eq!(v.intent.id, "prep-abc");
        assert_eq!(v.intent.step.as_deref(), Some("retest"));
    }

    #[test]
    fn signed_path_value_tamper_rejected() {
        let mut yaml = build_signed_prep("2026-05-13T20:00:00Z", "2026-05-13T12:00:00Z");
        // Swap instrument after signing.
        yaml = yaml.replace("instrument: EUR_USD", "instrument: GBPJPY");
        let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
        assert!(matches!(
            parse_and_verify(&yaml, &KEY, now),
            Err(IncomingError::Sig(SigError::Mismatch))
        ));
    }

    #[test]
    fn signed_path_added_field_rejected() {
        let yaml = build_signed_prep("2026-05-13T20:00:00Z", "2026-05-13T12:00:00Z");
        // Inject an extra field. Even if the worker would ignore it,
        // the schema fingerprint catches the addition.
        let yaml = yaml.replace("step: retest\n", "step: retest\nrisk_pct: 100.0\n");
        let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
        assert!(matches!(
            parse_and_verify(&yaml, &KEY, now),
            Err(IncomingError::Sig(SigError::Mismatch))
        ));
    }

    #[test]
    fn signed_path_dropped_shell_key_rejected() {
        let yaml = build_signed_prep("2026-05-13T20:00:00Z", "2026-05-13T12:00:00Z");
        // Drop close — values aren't signed but the key's PRESENCE is.
        let yaml = yaml
            .lines()
            .filter(|l| !l.starts_with("close:"))
            .collect::<Vec<_>>()
            .join("\n");
        let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
        // BadYaml is acceptable (Shell deser fails) — but more likely Sig::Mismatch
        // since we verify before deserialising. Both are rejections; assert it's not Ok.
        assert!(parse_and_verify(&yaml, &KEY, now).is_err());
    }

    #[test]
    fn signed_path_shell_value_substitution_ok() {
        // The CLI signs with the literal {{close}} placeholder; TV
        // substitutes a number before delivery. Worker must accept that.
        let pre_substitution = [
            "close: {{close}}",
            "high: {{high}}",
            "low: {{low}}",
            "time: \"{{time}}\"",
            "v: 1",
            "action: prep",
            "instrument: EUR_USD",
            "id: prep-abc",
            "not_after: \"2026-05-13T20:00:00Z\"",
            "step: retest",
            "ttl_hours: 12",
            "",
        ]
        .join("\n");
        let pairs = signed_pairs_from_text(&pre_substitution).unwrap();
        let sig = crate::sig::sign(&KEY, &pairs).unwrap();
        let on_wire = [
            "close: 1.1000",
            "high: 1.1020",
            "low: 1.0980",
            "time: \"2026-05-13T12:00:00Z\"",
            "v: 1",
            "action: prep",
            "instrument: EUR_USD",
            "id: prep-abc",
            "not_after: \"2026-05-13T20:00:00Z\"",
            "step: retest",
            "ttl_hours: 12",
            &format!("sig: \"{sig}\""),
            "",
        ]
        .join("\n");
        let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
        let v = parse_and_verify(&on_wire, &KEY, now).unwrap();
        assert_eq!(v.intent.id, "prep-abc");
    }

    #[test]
    fn signed_path_wrong_key_rejected() {
        let yaml = build_signed_prep("2026-05-13T20:00:00Z", "2026-05-13T12:00:00Z");
        let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
        let bad = [1u8; 32];
        assert!(matches!(
            parse_and_verify(&yaml, &bad, now),
            Err(IncomingError::Sig(SigError::Mismatch))
        ));
    }

    #[test]
    fn signed_path_expired_rejected() {
        let yaml = build_signed_prep("2026-05-13T11:59:00Z", "2026-05-13T11:00:00Z");
        let now: DateTime<Utc> = "2026-05-13T13:00:00Z".parse().unwrap();
        assert!(matches!(
            parse_and_verify(&yaml, &KEY, now),
            Err(IncomingError::Expired)
        ));
    }
}
