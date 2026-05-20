//! Signed YAML payload parsing.
//!
//! Wire format: flat cleartext YAML with the intent fields at the top
//! level alongside the TradingView shell (`close`, `high`, `low`,
//! `time`), plus a `sig: "v1-sig.<base64>"` field carrying the HMAC.
//! See [`crate::sig`] for the canonical form.
//!
//! Cleartext means the body shows up in Cloudflare's request log —
//! that's a deliberate trade-off for operator debugging. Auth is via
//! HMAC-SHA256 over the body; the request log is read-only.
//!
//! This module parses + verifies + sanity-checks timestamps. Replay
//! protection and cooldown checks live in the dispatch layer because
//! they need a `StateStore`.

use chrono::{DateTime, Utc};

use crate::intent::{Intent, IntentValidationError, Shell};
use crate::sig::{self, SIG_FIELD, SigError};

#[derive(Debug)]
pub enum IncomingError {
    BadYaml,
    /// Sig verification failed.
    Sig(SigError),
    BadIntentYaml,
    UnsupportedVersion(u32),
    /// `time` (from TradingView) is too far from `now`.
    StaleShellTime,
    /// `now < not_before`.
    TooEarly,
    /// `now > not_after`.
    Expired,
    /// Post-deser validation rejected the intent (e.g. malformed
    /// `trade_id`). Carries the underlying reason so the operator can
    /// see what failed without parsing free-form error text.
    InvalidIntent(IntentValidationError),
}

impl core::fmt::Display for IncomingError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadYaml => f.write_str("invalid YAML"),
            Self::Sig(e) => write!(f, "sig: {e}"),
            Self::BadIntentYaml => f.write_str("intent fields don't parse"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported intent version {v}"),
            Self::StaleShellTime => f.write_str("plaintext time too far from now"),
            Self::TooEarly => f.write_str("intent not yet valid"),
            Self::Expired => f.write_str("intent expired"),
            Self::InvalidIntent(e) => write!(f, "invalid intent: {e}"),
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

/// Parse + verify a signed body.
///
/// The signing input is built by **line scanning** the raw body text —
/// every line of the form `<key>: <value>` at the top level (no
/// indentation) becomes a `(key, value_raw)` pair. We sign the raw
/// textual value to avoid number-format / quoting drift between the
/// CLI's emit step and the worker's verify step.
///
/// Constraints on the wire format:
///   - One field per line at indent 0.
///   - Multi-field structures (e.g. enter's `entry`) serialise as inline
///     flow-style YAML on a single line: `entry: {type: market}`.
///   - `sig` MUST be the last line (or anywhere — we exclude it from
///     signing regardless).
pub fn parse_and_verify(
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
    let shell = Shell {
        close: mapping_f64(mapping, "close")?,
        high: mapping_f64(mapping, "high")?,
        low: mapping_f64(mapping, "low")?,
        time: mapping_time(mapping, "time")?,
    };

    check_intent_freshness(&shell, &intent, now)?;
    intent.validate().map_err(IncomingError::InvalidIntent)?;
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
    use chrono::Duration;

    const KEY: [u8; 32] = [9u8; 32];

    /// Helper to sign and assemble a generic signed body from a list of
    /// already-formatted lines (excluding `sig`). The signed-path tests
    /// then exercise specific failure modes by tampering with the
    /// returned body.
    fn signed_body(lines_without_sig: &[&str]) -> String {
        let body_without_sig = lines_without_sig.join("\n") + "\n";
        let pairs = signed_pairs_from_text(&body_without_sig).unwrap();
        let sig = crate::sig::sign(&KEY, &pairs).unwrap();
        format!("{body_without_sig}sig: \"{sig}\"\n")
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

    #[test]
    fn signed_path_stale_shell_time_rejected() {
        // shell time is 2 days before now — outside MAX_SHELL_SKEW_HOURS
        let yaml = build_signed_prep("2030-01-01T00:00:00Z", "2026-05-13T00:00:00Z");
        let now: DateTime<Utc> = "2026-05-15T00:00:01Z".parse().unwrap();
        assert!(matches!(
            parse_and_verify(&yaml, &KEY, now),
            Err(IncomingError::StaleShellTime)
        ));
    }

    #[test]
    fn signed_path_too_early_rejected() {
        // Intent with not_before in the future fails the freshness gate
        // before the dispatch layer would see it.
        let yaml = signed_body(&[
            "close: 1.1000",
            "high: 1.1020",
            "low: 1.0980",
            "time: \"2026-05-13T12:00:00Z\"",
            "v: 1",
            "action: prep",
            "instrument: EUR_USD",
            "id: prep-abc",
            "not_before: \"2026-05-13T13:00:00Z\"",
            "not_after: \"2026-05-13T20:00:00Z\"",
            "step: retest",
            "ttl_hours: 12",
        ]);
        let now: DateTime<Utc> = "2026-05-13T12:00:00Z".parse().unwrap();
        assert!(matches!(
            parse_and_verify(&yaml, &KEY, now),
            Err(IncomingError::TooEarly)
        ));
    }

    #[test]
    fn signed_path_invalid_trade_id_rejected() {
        // Valid sig + valid timestamps, but the trade_id is junk —
        // post-deser validation must reject it before the dispatch
        // layer would record it in the seen-index.
        let yaml = signed_body(&[
            "close: 1.1000",
            "high: 1.1020",
            "low: 1.0980",
            "time: \"2026-05-13T12:00:00Z\"",
            "v: 1",
            "action: status",
            "instrument: ALL",
            "id: status-bad-trade-id",
            "not_after: \"2026-05-13T20:00:00Z\"",
            "trade_id: BadCase",
        ]);
        let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
        assert!(matches!(
            parse_and_verify(&yaml, &KEY, now),
            Err(IncomingError::InvalidIntent(
                crate::intent::IntentValidationError::InvalidTradeId
            ))
        ));
    }

    #[test]
    fn signed_path_valid_trade_id_accepted() {
        // The happy path: well-formed trade_id round-trips through
        // parse_and_verify and lands on Verified intact.
        let yaml = signed_body(&[
            "close: 1.1000",
            "high: 1.1020",
            "low: 1.0980",
            "time: \"2026-05-13T12:00:00Z\"",
            "v: 1",
            "action: prep",
            "instrument: EUR_USD",
            "id: prep-tid-ok",
            "not_after: \"2026-05-13T20:00:00Z\"",
            "step: retest",
            "ttl_hours: 12",
            "trade_id: eurusd-short-01jb2x",
        ]);
        let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
        let v = parse_and_verify(&yaml, &KEY, now).unwrap();
        assert_eq!(v.intent.trade_id.as_deref(), Some("eurusd-short-01jb2x"));
    }

    #[test]
    fn signed_path_unsupported_version_rejected() {
        let yaml = signed_body(&[
            "close: 1.1000",
            "high: 1.1020",
            "low: 1.0980",
            "time: \"2026-05-13T12:00:00Z\"",
            "v: 99",
            "action: close",
            "instrument: EUR_USD",
            "id: prep-abc",
            "not_after: \"2030-01-01T00:00:00Z\"",
        ]);
        let now: DateTime<Utc> = "2026-05-13T12:00:00Z".parse().unwrap();
        assert!(matches!(
            parse_and_verify(&yaml, &KEY, now),
            Err(IncomingError::UnsupportedVersion(99))
        ));
    }
}
