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
    /// Top-level YAML parse / shape failure. Carries a short reason so
    /// operators can tell apart "not a mapping", a serde error message,
    /// or a malformed top-level pair without re-parsing free-form text.
    BadYaml(String),
    /// Sig verification failed.
    Sig(SigError),
    /// Intent fields didn't deserialise. Carries the serde error message.
    BadIntentYaml(String),
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
            Self::BadYaml(why) => write!(f, "invalid YAML: {why}"),
            Self::Sig(e) => write!(f, "sig: {e}"),
            Self::BadIntentYaml(why) => write!(f, "intent fields don't parse: {why}"),
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
        serde_yaml::from_str(yaml).map_err(|e| IncomingError::BadYaml(format!("from_str: {e}")))?;
    let mapping = value
        .as_mapping()
        .ok_or_else(|| IncomingError::BadYaml("top-level value is not a mapping".into()))?;

    let mut intent_map = serde_yaml::Mapping::new();
    let mut shell_map = serde_yaml::Mapping::new();
    for (k, v) in mapping {
        let key_str = k.as_str().unwrap_or("");
        match key_str {
            "sig" => continue,
            "close" | "high" | "low" | "time" | "signal_high" | "signal_low" | "signal_range"
            | "signal_start_time" | "signal_kind" | "golden" | "atr" | "signal_confirmed"
            | "recent_high" | "recent_low" => {
                shell_map.insert(k.clone(), v.clone());
            }
            _ => {
                intent_map.insert(k.clone(), v.clone());
            }
        }
    }
    let intent: Intent = serde_yaml::from_value(serde_yaml::Value::Mapping(intent_map))
        .map_err(|e| IncomingError::BadIntentYaml(e.to_string()))?;
    // Deserialise the Shell directly so its serde adapters
    // (signal_time_serde, signal_kind_serde, bool_one_zero_serde)
    // handle Pine's millisecond-int / float-code / 0-or-1 wire forms.
    let shell: Shell = serde_yaml::from_value(serde_yaml::Value::Mapping(shell_map))
        .map_err(|e| IncomingError::BadYaml(format!("shell deser: {e}")))?;

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
            return Err(IncomingError::BadYaml(format!(
                "empty key in top-level line: {line:?}"
            )));
        }
        let val = strip_yaml_quotes(v.trim()).to_string();
        out.push((key, val));
    }
    Ok(out)
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
    fn signed_path_signal_fields_round_trip() {
        // CLI signs with signal_* {{plot(...)}} placeholders; TV
        // substitutes Pine's wire forms: signal_start_time as a
        // millisecond integer, signal_kind as a float code, booleans
        // as 0/1. The Shell's serde adapters accept all of them.
        let pre_substitution = [
            "close: {{close}}",
            "high: {{high}}",
            "low: {{low}}",
            "time: \"{{time}}\"",
            "signal_high: {{plot(\"signal_high\")}}",
            "signal_low: {{plot(\"signal_low\")}}",
            "signal_range: {{plot(\"signal_range\")}}",
            "signal_start_time: {{plot(\"signal_start_time\")}}",
            "signal_kind: {{plot(\"signal_kind\")}}",
            "golden: {{plot(\"signal_golden\")}}",
            "atr: {{plot(\"signal_atr\")}}",
            "signal_confirmed: {{plot(\"signal_confirmed\")}}",
            "v: 1",
            "action: prep",
            "instrument: EUR_USD",
            "id: prep-sig",
            "not_after: \"2026-05-13T20:00:00Z\"",
            "step: retest",
            "ttl_hours: 12",
            "",
        ]
        .join("\n");
        let pairs = signed_pairs_from_text(&pre_substitution).unwrap();
        let sig = crate::sig::sign(&KEY, &pairs).unwrap();
        let on_wire = [
            "close: 1.16438",
            "high: 1.16440",
            "low: 1.16430",
            "time: \"2026-05-13T12:00:00Z\"",
            "signal_high: 1.16437",
            "signal_low: 1.16432",
            "signal_range: 0.00005",
            // ms epoch matches 2026-05-13T11:59:00Z (the prior signal bar).
            "signal_start_time: 1779728340000",
            // Pinbar = 1.
            "signal_kind: 1",
            "golden: 1",
            "atr: 0.00012",
            "signal_confirmed: 1",
            "v: 1",
            "action: prep",
            "instrument: EUR_USD",
            "id: prep-sig",
            "not_after: \"2026-05-13T20:00:00Z\"",
            "step: retest",
            "ttl_hours: 12",
            &format!("sig: \"{sig}\""),
            "",
        ]
        .join("\n");
        let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
        let v = parse_and_verify(&on_wire, &KEY, now).unwrap();
        assert_eq!(v.shell.signal_high, Some(1.16437));
        assert_eq!(v.shell.signal_low, Some(1.16432));
        assert_eq!(v.shell.signal_range, Some(0.00005));
        assert_eq!(v.shell.signal_kind, Some(crate::intent::SignalKind::Pinbar));
        assert_eq!(v.shell.golden, Some(true));
        assert_eq!(v.shell.atr, Some(0.00012));
        assert_eq!(v.shell.signal_confirmed, Some(true));
        let sig_time = v.shell.signal_start_time.unwrap();
        assert_eq!(sig_time.timestamp_millis(), 1779728340000);
    }

    #[test]
    fn signed_path_recent_high_low_substitution_ok() {
        // CLI signs with {{plot("recent_high")}} / {{plot("recent_low")}}
        // placeholders; TV substitutes real numbers. Sig must survive
        // (they're in UNSIGNED_VALUE_KEYS) and the Shell deser must
        // route them off the intent map.
        let pre_substitution = [
            "close: {{close}}",
            "high: {{high}}",
            "low: {{low}}",
            "time: \"{{time}}\"",
            "recent_high: {{plot(\"recent_high\")}}",
            "recent_low: {{plot(\"recent_low\")}}",
            "v: 1",
            "action: prep",
            "instrument: EUR_USD",
            "id: prep-recent",
            "not_after: \"2026-05-13T20:00:00Z\"",
            "step: retest",
            "ttl_hours: 12",
            "",
        ]
        .join("\n");
        let pairs = signed_pairs_from_text(&pre_substitution).unwrap();
        let sig = crate::sig::sign(&KEY, &pairs).unwrap();
        let on_wire = [
            "close: 1.16438",
            "high: 1.16440",
            "low: 1.16430",
            "time: \"2026-05-13T12:00:00Z\"",
            "recent_high: 1.16500",
            "recent_low: 1.16400",
            "v: 1",
            "action: prep",
            "instrument: EUR_USD",
            "id: prep-recent",
            "not_after: \"2026-05-13T20:00:00Z\"",
            "step: retest",
            "ttl_hours: 12",
            &format!("sig: \"{sig}\""),
            "",
        ]
        .join("\n");
        let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
        let v = parse_and_verify(&on_wire, &KEY, now).unwrap();
        assert_eq!(v.shell.recent_high, Some(1.16500));
        assert_eq!(v.shell.recent_low, Some(1.16400));
    }

    #[test]
    fn signed_path_signal_confirmed_zero_is_false() {
        let pre_substitution = [
            "close: {{close}}",
            "high: {{high}}",
            "low: {{low}}",
            "time: \"{{time}}\"",
            "signal_confirmed: {{plot(\"signal_confirmed\")}}",
            "v: 1",
            "action: prep",
            "instrument: EUR_USD",
            "id: prep-sig0",
            "not_after: \"2026-05-13T20:00:00Z\"",
            "step: retest",
            "ttl_hours: 12",
            "",
        ]
        .join("\n");
        let pairs = signed_pairs_from_text(&pre_substitution).unwrap();
        let sig = crate::sig::sign(&KEY, &pairs).unwrap();
        let on_wire = [
            "close: 1.0",
            "high: 1.0",
            "low: 1.0",
            "time: \"2026-05-13T12:00:00Z\"",
            "signal_confirmed: 0",
            "v: 1",
            "action: prep",
            "instrument: EUR_USD",
            "id: prep-sig0",
            "not_after: \"2026-05-13T20:00:00Z\"",
            "step: retest",
            "ttl_hours: 12",
            &format!("sig: \"{sig}\""),
            "",
        ]
        .join("\n");
        let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
        let v = parse_and_verify(&on_wire, &KEY, now).unwrap();
        assert_eq!(v.shell.signal_confirmed, Some(false));
        assert_eq!(v.shell.signal_high, None);
        assert_eq!(v.shell.signal_start_time, None);
    }

    #[test]
    fn signed_path_signal_kind_codes_map_to_variants() {
        // Sanity sweep: every Pine KIND_* code maps to the right enum
        // variant. The wire is a float, but Pine emits integer values
        // so we send `1`..`5` and verify.
        use crate::intent::SignalKind;
        for (code, expected) in [
            (1u8, SignalKind::Pinbar),
            (2, SignalKind::Tweezer),
            (3, SignalKind::RegularEngulfer),
            (4, SignalKind::FloatingEngulfer),
            (5, SignalKind::DoubleTweezer),
        ] {
            let pre_substitution = [
                "close: {{close}}",
                "high: {{high}}",
                "low: {{low}}",
                "time: \"{{time}}\"",
                "signal_kind: {{plot(\"signal_kind\")}}",
                "v: 1",
                "action: prep",
                "instrument: EUR_USD",
                "id: prep-kind",
                "not_after: \"2026-05-13T20:00:00Z\"",
                "step: retest",
                "ttl_hours: 12",
                "",
            ]
            .join("\n");
            let pairs = signed_pairs_from_text(&pre_substitution).unwrap();
            let sig = crate::sig::sign(&KEY, &pairs).unwrap();
            let on_wire = [
                "close: 1.0".to_string(),
                "high: 1.0".to_string(),
                "low: 1.0".to_string(),
                "time: \"2026-05-13T12:00:00Z\"".to_string(),
                format!("signal_kind: {code}"),
                "v: 1".to_string(),
                "action: prep".to_string(),
                "instrument: EUR_USD".to_string(),
                "id: prep-kind".to_string(),
                "not_after: \"2026-05-13T20:00:00Z\"".to_string(),
                "step: retest".to_string(),
                "ttl_hours: 12".to_string(),
                format!("sig: \"{sig}\""),
                String::new(),
            ]
            .join("\n");
            let now: DateTime<Utc> = "2026-05-13T12:01:00Z".parse().unwrap();
            let v = parse_and_verify(&on_wire, &KEY, now).unwrap();
            assert_eq!(v.shell.signal_kind, Some(expected));
        }
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
