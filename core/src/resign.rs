//! Re-serialise an already-[`Verified`] intent+shell back into signed wire
//! bytes, so a placement that never arrived on the wire is still *recoverable*.
//!
//! # Why this exists
//!
//! [`pending_lifecycle`](crate::pending_lifecycle) restores a resting order it
//! cancelled by **re-driving the signed intent** behind it. The live
//! [`SignedBodySource`](crate::pending_lifecycle::SignedBodySource) recovers
//! that intent by `parse_and_verify`-ing the body stored under `order:{id}`, so
//! an order with no stored body is `Recovered::Unrecoverable` — and RAIL 2
//! ("never cancel what we can't restore") correctly leaves it resting.
//!
//! The webhook path has the operator's signed bytes in hand and stores them. The
//! **engine** path does not: `trade-control-cron::engine::dispatch_action`
//! reconstructs each fired intent from a registered [`TradePlan`] and passes
//! `raw_body: None`. So every engine-placed order — the bulk of automated
//! pattern trading — was unrecoverable, and therefore got **no news-pause hold
//! and no market-hours hold**. The order was never lost; it simply sat through
//! windows it should have been pulled from. A missing protection, not a stranded
//! order.
//!
//! # Why re-signing mints no new authority
//!
//! The obvious objection is that the worker now signs instructions *it generated
//! itself*. Two facts make this a re-expression of existing authority rather than
//! a new grant:
//!
//! 1. **The input is already `Verified`.** [`resign`] takes a
//!    [`Verified`] — the type `parse_and_verify` *produces*. It is unreachable
//!    for unauthenticated input: to get one you either verified an operator's
//!    HMAC, or the engine built one from a plan the operator signed at register.
//!    Nothing here upgrades untrusted bytes into trusted ones.
//!
//! 2. **The stored plan is already trusted at rest, unsigned.**
//!    `StateStore::put_trade_plan` persists the `TradePlan` as plain JSONB;
//!    `TradePlan` has no `sig` field. The register envelope's signature is
//!    verified once, at the HTTP edge, and then *discarded*. So the engine
//!    already reads its instructions from an unauthenticated database row on
//!    every tick — signing the re-serialisation of one of those instructions
//!    does not lower the bar, it raises it: the `order:{id}` row gains a tamper
//!    check it did not previously have.
//!
//! An attacker who can write the `order:{id}` row can also write the `plan` row,
//! and the latter is both unsigned and strictly more powerful (it authors *all*
//! of a trade's future rules, not one already-placed order's restore). So this
//! is not the weakest link, and removing it would not shrink the trust boundary.
//!
//! # The confinement that keeps it narrow
//!
//! Re-signed bytes are still *indistinguishable* from operator bytes to anything
//! that merely checks the HMAC, so the confinement is structural rather than
//! cryptographic:
//!
//! - The output is only ever written to the `order:{id}` namespace, by the one
//!   call site in [`crate::dispatch::enter`], and only for an order the broker
//!   has **already accepted**. It is never returned to a caller, never logged as
//!   bytes, and never placed anywhere the inbound webhook path reads.
//! - The inbound path ([`crate::incoming::parse_and_verify`] at the HTTP edge)
//!   reads the *request body*, which this never touches. There is no code path
//!   by which a re-signed body can present itself as an inbound instruction.
//! - `id` and `not_after` are carried through **unchanged**, so a re-signed body
//!   inherits the original's replay-protection identity and expiry window. It
//!   cannot outlive, or re-fire beyond, what the operator authorised: a restore
//!   after `not_after` is `Recovered::Expired` and the order is dropped.
//!
//! # Fidelity requirement
//!
//! The emitted bytes MUST round-trip through `parse_and_verify` — the wire
//! format is line-scanned (one top-level `key: value` per line, nested values in
//! flow style), not YAML-parsed, for signing. [`resign`] therefore builds the
//! body and then signs the *line-scan view of its own output*, exactly as the
//! CLI's emit step does, so the pair list signed here is by construction the one
//! the verify side reconstructs.

use crate::incoming::{self, Verified};
use crate::sig;

/// Why a [`resign`] call could not produce wire bytes. Every variant is a
/// programming/serialisation fault rather than an operator error — none is
/// reachable from untrusted input — so callers log and degrade rather than
/// surfacing these to a user.
#[derive(Debug)]
pub enum ResignError {
    /// `Intent` or `Shell` did not serialise to a YAML mapping.
    Serialise(String),
    /// The emitted body did not line-scan back into signable pairs.
    Pairs(String),
    /// HMAC signing failed (bad key length).
    Sign(sig::SigError),
}

impl core::fmt::Display for ResignError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Serialise(why) => write!(f, "serialise: {why}"),
            Self::Pairs(why) => write!(f, "line-scan: {why}"),
            Self::Sign(e) => write!(f, "sign: {e}"),
        }
    }
}

impl std::error::Error for ResignError {}

/// Re-serialise `verified` into signed wire bytes that `parse_and_verify` will
/// accept under the same `key`.
///
/// The output is byte-shaped like an operator alert: shell fields and intent
/// fields as top-level single-line `key: value` pairs, `sig` last. See the
/// module docs for why this mints no authority the operator did not already
/// grant, and for the confinement rules the single call site must preserve.
pub fn resign(verified: &Verified, key: &[u8]) -> Result<String, ResignError> {
    let mut lines = Vec::new();
    // Shell first, then intent — mirroring the CLI's emit order. Order is
    // cosmetic (the canonical signing form sorts by key), but keeping it stable
    // makes a stored body diffable against an operator one.
    push_mapping_lines(&mut lines, verified.shell.clone())?;
    push_mapping_lines(&mut lines, verified.intent.clone())?;

    let body_without_sig = format!("{}\n", lines.join("\n"));
    // Sign the line-scan view of OUR OWN output, not the structs we started
    // from: that is what guarantees the signed pair list is exactly the one the
    // verify side will reconstruct from these bytes.
    let pairs = incoming::signed_pairs_from_text(&body_without_sig)
        .map_err(|e| ResignError::Pairs(e.to_string()))?;
    let signature = sig::sign(key, &pairs).map_err(ResignError::Sign)?;
    Ok(format!("{body_without_sig}sig: \"{signature}\"\n"))
}

/// Serialise one `Serialize` value to a YAML mapping and push each top-level
/// entry as a single `key: value` line.
fn push_mapping_lines<T: serde::Serialize>(
    lines: &mut Vec<String>,
    value: T,
) -> Result<(), ResignError> {
    let yaml = serde_yaml::to_string(&value)
        .map_err(|e| ResignError::Serialise(format!("to_string: {e}")))?;
    let parsed: serde_yaml::Value =
        serde_yaml::from_str(&yaml).map_err(|e| ResignError::Serialise(format!("reparse: {e}")))?;
    let mapping = parsed
        .as_mapping()
        .ok_or_else(|| ResignError::Serialise("value is not a mapping".into()))?;
    for (k, v) in mapping {
        let key = k
            .as_str()
            .ok_or_else(|| ResignError::Serialise("non-string key".into()))?;
        lines.push(format!("{key}: {}", render_value(v)?));
    }
    Ok(())
}

/// Render a YAML value onto one line: scalars raw, nested values in flow style.
///
/// Modelled on the CLI's emit step (`cli/src/control.rs`) but deliberately not
/// shared with it — the CLI signs operator input and this signs engine output,
/// and the two must be free to diverge without one silently changing the other's
/// bytes. The round-trip tests below are what keep them compatible.
fn render_value(v: &serde_yaml::Value) -> Result<String, ResignError> {
    match v {
        serde_yaml::Value::Null => Ok("~".to_string()),
        serde_yaml::Value::Bool(b) => Ok(b.to_string()),
        serde_yaml::Value::Number(n) => Ok(n.to_string()),
        // Quoting is safe for any string — the line-scan strips matching quotes
        // — so quote anything that could confuse it rather than guessing.
        serde_yaml::Value::String(s) if needs_quoting(s) => {
            Ok(format!("\"{}\"", s.replace('"', "\\\"")))
        }
        serde_yaml::Value::String(s) => Ok(s.clone()),
        // Nested structures MUST stay on one line — `serde_yaml::to_string`
        // emits block style, whose indented continuation lines are invisible to
        // the top-level line-scan, so the body would verify while silently
        // dropping (say) an entry's trigger price. Rendered by hand rather than
        // via `serde_json`, which `core` carries only under `test-support` and
        // deliberately keeps out of the worker release build.
        serde_yaml::Value::Sequence(items) => {
            let rendered: Result<Vec<_>, _> = items.iter().map(render_value).collect();
            Ok(format!("[{}]", rendered?.join(", ")))
        }
        serde_yaml::Value::Mapping(map) => {
            let rendered: Result<Vec<_>, _> = map
                .iter()
                .map(|(k, v)| {
                    let key = render_value(k)?;
                    let val = render_value(v)?;
                    Ok(format!("{key}: {val}"))
                })
                .collect();
            Ok(format!("{{{}}}", rendered?.join(", ")))
        }
        serde_yaml::Value::Tagged(t) => {
            // A tagged node's own tag is part of the value's meaning; dropping
            // it would change what the body says. `Intent`'s wire forms are
            // untagged, so this is unreachable today — fail loudly rather than
            // emitting something that differs from what was placed.
            Err(ResignError::Serialise(format!(
                "tagged value cannot be rendered on one line: {:?}",
                t.tag
            )))
        }
    }
}

/// Does this string need quoting to survive the single-line line-scan?
fn needs_quoting(s: &str) -> bool {
    s.is_empty()
        || s.chars()
            .any(|c| matches!(c, ':' | '#' | '\'' | '"' | '\n'))
        || s.starts_with(char::is_whitespace)
        || s.ends_with(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::Candle;
    use crate::intent::{Intent, Shell};
    use chrono::{DateTime, Utc};

    const KEY: [u8; 32] = [9u8; 32];

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    /// A realistic engine-built enter: the shape `dispatch_fired` hands to
    /// `run_enter`, with a nested `entry` (the flow-style case) and the baked
    /// `pip_size` the lifecycle's pips math needs.
    fn engine_verified() -> Verified {
        let intent: Intent = serde_json::from_str(
            r#"{
                "v": 1,
                "id": "t-enter",
                "not_after": "2026-07-09T00:00:00Z",
                "action": "enter",
                "instrument": "AUD/CHF",
                "direction": "short",
                "entry": { "type": "stop", "from": "close", "offset_pips": 0.0, "at": 0.5598 },
                "stop_loss": { "absolute": 0.5607 },
                "take_profit": { "absolute": 0.5560 },
                "broker": "tradenation",
                "trade_id": "t",
                "pip_size": 0.0001
            }"#,
        )
        .expect("valid enter intent");
        let shell = Shell::from_candle(&Candle {
            time: ts("2026-07-08T20:00:00Z"),
            o: 0.5600,
            h: 0.5605,
            l: 0.5595,
            c: 0.5600,
        });
        Verified { shell, intent }
    }

    /// THE CONTRACT: re-signed bytes verify under the same key, and the intent
    /// that comes back out is the one that went in. This is what the live
    /// `SignedBodySource` does on every restore.
    #[test]
    fn resigned_body_verifies_and_round_trips() {
        let v = engine_verified();
        let body = resign(&v, &KEY).expect("resign");
        let now = ts("2026-07-08T20:05:00Z");

        let back = incoming::parse_and_verify(&body, &KEY, now).expect("re-verify");

        assert_eq!(back.intent.id, v.intent.id);
        assert_eq!(back.intent.action, v.intent.action);
        assert_eq!(back.intent.instrument, v.intent.instrument);
        assert_eq!(back.intent.trade_id, v.intent.trade_id);
        assert_eq!(back.intent.not_after, v.intent.not_after);
        assert_eq!(back.intent.pip_size, v.intent.pip_size);
        assert_eq!(back.shell.time, v.shell.time);
        assert_eq!(back.shell.close, v.shell.close);
    }

    /// The nested `entry` structure must survive as flow style on ONE line.
    /// Block-style YAML here would make the indented lines invisible to the
    /// line-scan, so the body would verify while silently carrying a DIFFERENT
    /// entry than the order that was placed.
    #[test]
    fn nested_entry_survives_as_one_line() {
        let v = engine_verified();
        let body = resign(&v, &KEY).expect("resign");
        let entry_line = body
            .lines()
            .find(|l| l.starts_with("entry:"))
            .expect("an entry line");
        assert!(
            entry_line.contains("0.5598"),
            "entry trigger must be on the entry line itself: {entry_line}"
        );
        let back =
            incoming::parse_and_verify(&body, &KEY, ts("2026-07-08T20:05:00Z")).expect("re-verify");
        assert_eq!(
            back.intent.entry.as_ref().map(|e| format!("{e:?}")),
            v.intent.entry.as_ref().map(|e| format!("{e:?}")),
            "the resolved entry must survive the round trip byte-for-byte",
        );
    }

    /// A body signed with one key must NOT verify under another. Proves the
    /// signature is real rather than a constant the verify side ignores.
    #[test]
    fn a_different_key_does_not_verify() {
        let body = resign(&engine_verified(), &KEY).expect("resign");
        let other = [1u8; 32];
        assert!(
            incoming::parse_and_verify(&body, &other, ts("2026-07-08T20:05:00Z")).is_err(),
            "a foreign key must never verify a re-signed body",
        );
    }

    /// Tampering with a signed value after signing must break verification —
    /// the tamper-check the `order:{id}` row gains from this change.
    #[test]
    fn tampering_with_a_field_breaks_verification() {
        let body = resign(&engine_verified(), &KEY).expect("resign");
        let tampered = body.replace("instrument: AUD/CHF", "instrument: EUR/USD");
        assert_ne!(
            tampered, body,
            "the fixture must actually contain the field"
        );
        assert!(
            incoming::parse_and_verify(&tampered, &KEY, ts("2026-07-08T20:05:00Z")).is_err(),
            "a tampered re-signed body must not verify",
        );
    }

    /// `not_after` is carried through unchanged, so a re-signed body cannot
    /// outlive what the operator authorised: past the window it is `Expired`,
    /// which the restore path treats as "drop the order", not "re-place it".
    #[test]
    fn a_resigned_body_expires_with_the_original_window() {
        let v = engine_verified();
        let body = resign(&v, &KEY).expect("resign");
        // One second past the intent's own `not_after`.
        let past = v.intent.not_after + chrono::Duration::seconds(1);
        assert!(
            matches!(
                incoming::parse_and_verify(&body, &KEY, past),
                Err(incoming::IncomingError::Expired)
            ),
            "a re-signed body must expire at the original not_after",
        );
    }
}
