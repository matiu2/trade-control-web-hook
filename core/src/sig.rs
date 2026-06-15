//! HMAC signing for the cleartext webhook payload.
//!
//! Why not encryption? The plaintext intent isn't secret — only its
//! authenticity matters. Signing the body and leaving it readable means
//! the Cloudflare request log shows exactly what TradingView sent, which
//! makes operator debugging vastly easier than the encrypt path.
//!
//! ## Canonical form
//!
//! The signature covers:
//!   1. A fixed protocol tag (`v1-sig`) so versions can't be cross-replayed.
//!   2. A sorted list of all top-level YAML keys (the **schema fingerprint**).
//!      An attacker can't add, remove, or rename a top-level field without
//!      invalidating the sig — they'd have to know the key.
//!   3. The values of all *signed* keys, sorted, one `key=value` per line.
//!
//! Signed values exclude the TradingView shell (`close`, `high`, `low`,
//! `time`, `signal_high`, `signal_low`, `signal_range`,
//! `signal_start_time`, `signal_kind`, `golden`, `atr`,
//! `signal_confirmed`) — TradingView fills those *after* the CLI emits
//! the body via `{{close}}` / `{{plot("signal_high")}}` etc., so their
//! values can't be known at sign time. Their *presence* still matters
//! and is covered by the schema fingerprint. `sig` is always excluded
//! from the values (it's the output, not the input).
//!
//! ## Wire form
//!
//! The body is flat cleartext YAML, e.g.:
//!
//! ```yaml
//! close: {{close}}
//! high: {{high}}
//! low: {{low}}
//! time: "{{time}}"
//! v: 1
//! action: prep
//! instrument: GBPJPY
//! id: GBPJPY-{{time}}-retest
//! step: retest
//! ttl_hours: 12
//! not_after: "2026-05-20T10:55:32+00:00"
//! sig: "v1-sig.<base64-hmac>"
//! ```

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// Sig wire prefix. Distinct from the `v1.` AEAD prefix so a downgrade
/// attempt can be spotted immediately.
pub const PREFIX: &str = "v1-sig.";

/// Protocol tag mixed into the HMAC. Bumping is a hard break.
const AAD: &[u8] = b"trade-control-sig-v1";

/// Keys whose values are NOT signed (TradingView fills them after the
/// CLI emits the body). The keys themselves still appear in the schema
/// fingerprint, so an attacker can't drop them. The signal_* / golden /
/// atr keys are substituted from the candle-signals-v2.pine indicator's
/// hidden plots via `{{plot("signal_high")}}` etc.
const UNSIGNED_VALUE_KEYS: &[&str] = &[
    "close",
    "high",
    "low",
    "open",
    "time",
    "signal_high",
    "signal_low",
    "signal_range",
    "signal_start_time",
    "signal_kind",
    "golden",
    "atr",
    "signal_confirmed",
    "recent_high",
    "recent_low",
    "next_candle_timestamp_1",
    "next_candle_timestamp_2",
    "next_candle_timestamp_3",
    "next_candle_timestamp_4",
    "next_candle_timestamp_5",
];

/// Field name on the wire that holds the signature itself.
pub const SIG_FIELD: &str = "sig";

/// 32-byte HMAC key length.
pub const KEY_LEN: usize = 32;

/// Parse a 64-hex-char string into a 32-byte signing key. Trimmed of
/// whitespace; either case is accepted. Used by both the CLI (when
/// loading `key.hex`) and the worker (when reading the `SIGNING_KEY`
/// secret).
pub fn parse_key_hex(s: &str) -> Result<[u8; KEY_LEN], SigError> {
    let trimmed = s.trim();
    let bytes = hex::decode(trimmed).map_err(|_| SigError::BadKeyLen)?;
    let arr: [u8; KEY_LEN] = bytes.try_into().map_err(|_| SigError::BadKeyLen)?;
    Ok(arr)
}

#[derive(Debug)]
pub enum SigError {
    BadKeyLen,
    BadPrefix,
    BadBase64,
    /// HMAC mismatch — body was tampered with, or signed under a
    /// different key.
    Mismatch,
    /// Body claims a sig but the field is missing or not a string.
    MissingSig,
}

impl core::fmt::Display for SigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::BadKeyLen => "key must be 32 bytes",
            Self::BadPrefix => "sig missing v1-sig. prefix",
            Self::BadBase64 => "sig is not valid base64",
            Self::Mismatch => "signature does not verify",
            Self::MissingSig => "body has no sig field",
        };
        f.write_str(s)
    }
}

impl std::error::Error for SigError {}

/// Build the canonical signing input from a list of `(key, value)` pairs.
///
/// `pairs` is the full set of top-level fields *including* the shell
/// fields and excluding only `sig` itself. The function sorts keys
/// alphabetically and writes:
///
/// ```text
/// v1-sig
/// keys:close,high,id,instrument,...
/// action=prep
/// id=GBPJPY-...
/// ...
/// ```
///
/// Lines are `\n`-terminated. Values for keys in [`UNSIGNED_VALUE_KEYS`]
/// are omitted from the value lines (but present in the keys list).
pub fn canonical_form(pairs: &[(String, String)]) -> Vec<u8> {
    let mut keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();

    let mut signed_pairs: Vec<(&str, &str)> = pairs
        .iter()
        .filter(|(k, _)| !UNSIGNED_VALUE_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    signed_pairs.sort_by(|a, b| a.0.cmp(b.0));

    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(b"v1-sig\n");
    out.extend_from_slice(b"keys:");
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(k.as_bytes());
    }
    out.push(b'\n');
    for (k, v) in signed_pairs {
        out.extend_from_slice(k.as_bytes());
        out.push(b'=');
        out.extend_from_slice(v.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Compute the `v1-sig.<base64>` signature string for the given pairs.
pub fn sign(key: &[u8], pairs: &[(String, String)]) -> Result<String, SigError> {
    if key.len() != 32 {
        return Err(SigError::BadKeyLen);
    }
    let canon = canonical_form(pairs);
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).map_err(|_| SigError::BadKeyLen)?;
    mac.update(AAD);
    mac.update(&canon);
    let tag = mac.finalize().into_bytes();
    Ok(format!("{PREFIX}{}", BASE64.encode(tag)))
}

/// Verify `sig` against `pairs`. Constant-time comparison.
///
/// `pairs` MUST exclude the `sig` field itself but include every other
/// top-level field (including the shell fields whose values are skipped
/// inside [`canonical_form`]).
pub fn verify(key: &[u8], pairs: &[(String, String)], sig: &str) -> Result<(), SigError> {
    if key.len() != 32 {
        return Err(SigError::BadKeyLen);
    }
    let body = sig.strip_prefix(PREFIX).ok_or(SigError::BadPrefix)?;
    let provided = BASE64.decode(body).map_err(|_| SigError::BadBase64)?;
    let canon = canonical_form(pairs);
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).map_err(|_| SigError::BadKeyLen)?;
    mac.update(AAD);
    mac.update(&canon);
    let expected = mac.finalize().into_bytes();
    if expected.ct_eq(&provided).into() {
        Ok(())
    } else {
        Err(SigError::Mismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7u8; 32];

    #[test]
    fn parse_key_hex_round_trip() {
        let key = [0x42u8; KEY_LEN];
        let hex_str = hex::encode(key);
        assert_eq!(parse_key_hex(&hex_str).unwrap(), key);
    }

    #[test]
    fn parse_key_hex_rejects_wrong_length() {
        assert!(parse_key_hex("dead").is_err());
    }

    #[test]
    fn parse_key_hex_rejects_non_hex() {
        assert!(parse_key_hex("not-hex-at-all-not-hex-at-all-not-hex-at-all-not-hex").is_err());
    }

    fn pairs() -> Vec<(String, String)> {
        vec![
            ("v".into(), "1".into()),
            ("action".into(), "prep".into()),
            ("instrument".into(), "GBPJPY".into()),
            ("id".into(), "GBPJPY-2026-05-18-retest".into()),
            ("step".into(), "retest".into()),
            ("ttl_hours".into(), "12".into()),
            ("not_after".into(), "2026-05-20T10:55:32+00:00".into()),
            ("close".into(), "1.2345".into()),
            ("high".into(), "1.2350".into()),
            ("low".into(), "1.2340".into()),
            ("time".into(), "2026-05-18T10:00:00Z".into()),
        ]
    }

    #[test]
    fn round_trip() {
        let sig = sign(&KEY, &pairs()).unwrap();
        assert!(sig.starts_with("v1-sig."));
        verify(&KEY, &pairs(), &sig).unwrap();
    }

    #[test]
    fn wrong_key_rejected() {
        let sig = sign(&KEY, &pairs()).unwrap();
        let wrong = [8u8; 32];
        assert!(matches!(
            verify(&wrong, &pairs(), &sig),
            Err(SigError::Mismatch)
        ));
    }

    #[test]
    fn value_tamper_rejected() {
        let sig = sign(&KEY, &pairs()).unwrap();
        let mut tampered = pairs();
        for p in tampered.iter_mut() {
            if p.0 == "instrument" {
                p.1 = "EUR_USD".into();
            }
        }
        assert!(matches!(
            verify(&KEY, &tampered, &sig),
            Err(SigError::Mismatch)
        ));
    }

    #[test]
    fn shell_value_change_does_not_break_sig() {
        // `close`/`high`/`low`/`time` values change between sign-time
        // (CLI emits {{close}}) and verify-time (TV fills in 1.2345).
        // The sig must survive that substitution.
        let sig = sign(&KEY, &pairs()).unwrap();
        let mut filled = pairs();
        for p in filled.iter_mut() {
            match p.0.as_str() {
                "close" => p.1 = "1.2999".into(),
                "high" => p.1 = "1.3000".into(),
                "low" => p.1 = "1.2900".into(),
                "time" => p.1 = "2026-05-18T11:00:00Z".into(),
                _ => {}
            }
        }
        verify(&KEY, &filled, &sig).unwrap();
    }

    #[test]
    fn dropping_a_shell_key_breaks_sig() {
        // Even though `close`'s VALUE isn't signed, its PRESENCE is —
        // via the schema fingerprint. Removing it is tampering.
        let sig = sign(&KEY, &pairs()).unwrap();
        let stripped: Vec<_> = pairs().into_iter().filter(|(k, _)| k != "close").collect();
        assert!(matches!(
            verify(&KEY, &stripped, &sig),
            Err(SigError::Mismatch)
        ));
    }

    #[test]
    fn adding_a_key_breaks_sig() {
        let sig = sign(&KEY, &pairs()).unwrap();
        let mut extended = pairs();
        extended.push(("risk_pct".into(), "100".into()));
        assert!(matches!(
            verify(&KEY, &extended, &sig),
            Err(SigError::Mismatch)
        ));
    }

    #[test]
    fn renaming_a_key_breaks_sig() {
        let sig = sign(&KEY, &pairs()).unwrap();
        let mut renamed = pairs();
        for p in renamed.iter_mut() {
            if p.0 == "step" {
                p.0 = "setp".into();
            }
        }
        assert!(matches!(
            verify(&KEY, &renamed, &sig),
            Err(SigError::Mismatch)
        ));
    }

    #[test]
    fn bad_prefix_rejected() {
        assert!(matches!(
            verify(&KEY, &pairs(), "v1.notasig"),
            Err(SigError::BadPrefix)
        ));
    }

    #[test]
    fn bad_base64_rejected() {
        assert!(matches!(
            verify(&KEY, &pairs(), "v1-sig.!!!"),
            Err(SigError::BadBase64)
        ));
    }

    #[test]
    fn bad_key_length_rejected() {
        let short = [0u8; 16];
        assert!(matches!(sign(&short, &pairs()), Err(SigError::BadKeyLen)));
    }

    #[test]
    fn canonical_form_is_stable_under_pair_reordering() {
        let a = sign(&KEY, &pairs()).unwrap();
        let mut reversed = pairs();
        reversed.reverse();
        let b = sign(&KEY, &reversed).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn canonical_form_excludes_shell_values() {
        let canon = canonical_form(&pairs());
        let s = String::from_utf8(canon).unwrap();
        // Schema fingerprint includes close
        assert!(s.contains("keys:") && s.contains("close"));
        // But no `close=1.2345` line
        assert!(!s.contains("close=1.2345"));
        assert!(!s.contains("high=1.2350"));
        assert!(!s.contains("time=2026-05-18T10:00:00Z"));
        // Signed fields ARE present as lines
        assert!(s.contains("instrument=GBPJPY"));
        assert!(s.contains("step=retest"));
    }

    #[test]
    fn recent_high_low_value_changes_do_not_break_sig() {
        // recent_high / recent_low are TV-substituted shell fields (Pine
        // emits them via {{plot(...)}}). Their values must be excluded
        // from signing so placeholder-at-sign-time vs real-number-at-wire
        // doesn't break verification.
        let mut signed_with_placeholders = pairs();
        signed_with_placeholders.push(("recent_high".into(), "{{plot(\"recent_high\")}}".into()));
        signed_with_placeholders.push(("recent_low".into(), "{{plot(\"recent_low\")}}".into()));
        let sig = sign(&KEY, &signed_with_placeholders).unwrap();

        let mut filled = signed_with_placeholders.clone();
        for p in filled.iter_mut() {
            match p.0.as_str() {
                "recent_high" => p.1 = "1.2500".into(),
                "recent_low" => p.1 = "1.2400".into(),
                _ => {}
            }
        }
        verify(&KEY, &filled, &sig).unwrap();
    }
}
