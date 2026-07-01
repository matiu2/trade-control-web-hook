//! Pure recording types shared by both runtimes' request recorders.
//!
//! These are the **backend-agnostic** parts of per-request recording: the
//! captured log line ([`LogLine`]), the whole-request record
//! ([`RequestRecord`]) with its R2-key formatter, and the two pure helpers
//! that mint a correlation id ([`mint_request_id`]) and extract the
//! intent/trade ids from a signed body ([`ids_from_body`]).
//!
//! They live in `core` so the two recorders — the wasm worker's R2 writer and
//! the native runtime's Postgres insert — build the *same* [`RequestRecord`]
//! and a future parity gate can diff them apples-to-apples. Everything here is
//! pure (serde + hashing + string parsing); no `worker::`, wasm-safe.
//!
//! The runtime-coupled pieces (the `rlog!`/`rlog_err!` macros, the
//! thread-local log buffer, R2 / Postgres I/O) live with each runtime, not
//! here.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

/// One captured log line.
///
/// `level` is a `Cow<'static, str>` so the write path can keep using the cheap
/// `&'static "log"` / `"error"` literals while the read path (`plan timeline`)
/// can still *deserialize* recorded records back — a borrowed `&'static str`
/// alone cannot derive `Deserialize`. Serialized wire shape is an ordinary
/// string either way.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogLine {
    /// `"log"` or `"error"` — mirrors console.log vs console.error.
    pub level: Cow<'static, str>,
    /// The formatted message.
    pub msg: String,
}

/// The full per-request record. The wasm worker writes it to R2 as one JSON
/// object; the native runtime inserts it into the `request_records` table as
/// `jsonb` (plus the extracted correlation columns).
#[derive(Debug, Serialize, Deserialize)]
pub struct RequestRecord {
    /// UTC RFC3339 — when the request was received.
    pub ts: String,
    /// The request id minted for this invocation (correlates logs).
    pub request_id: String,
    /// HTTP method + path.
    pub method: String,
    pub path: String,
    /// Request headers (verbatim).
    pub headers: Vec<(String, String)>,
    /// The verbatim request body (signed YAML for intents).
    pub body: String,
    /// The intent id, if the body parsed to a verified intent.
    pub intent_id: Option<String>,
    /// The trade id, if present on the intent.
    pub trade_id: Option<String>,
    /// Final HTTP status returned to the caller.
    pub status: u16,
    /// Short outcome string (e.g. `"entered"`, `"rejected: missing-prep"`).
    pub outcome: String,
    /// Every log line emitted during dispatch, in order.
    pub logs: Vec<LogLine>,
}

impl RequestRecord {
    /// R2 object key. Layout: `req/<date>/<ts>-<request_id>.json` so a
    /// day's requests list under one prefix and sort by time. Trade-keyed
    /// reconstruction filters on the `trade_id` field, not the key.
    ///
    /// Pure string format — harmless to keep in `core` even though only the
    /// wasm R2 writer uses it.
    pub fn r2_key(&self) -> String {
        // ts is RFC3339 like 2026-06-15T07:51:39.123Z; take the date.
        let date = self.ts.get(..10).unwrap_or("unknown");
        format!(
            "req/{date}/{ts}-{rid}.json",
            ts = self.ts,
            rid = self.request_id
        )
    }
}

/// Mint a short request id. We have no RNG in the wasm worker
/// (`Math::random`/`Date::now` are restricted in some contexts), so derive
/// a stable-but-unique id from a cheap hash of the body + headers. Two
/// distinct requests almost never collide; identical replays of the same
/// body intentionally hash the same, which is fine (the ts disambiguates
/// the R2 key).
pub fn mint_request_id(body: &str, headers: &[(String, String)]) -> String {
    // FNV-1a over body then header bytes. Not cryptographic — just a
    // compact correlation handle for logs and the R2 key.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    mix(body.as_bytes());
    for (k, v) in headers {
        mix(k.as_bytes());
        mix(v.as_bytes());
    }
    format!("{h:016x}")
}

/// Best-effort extract `id:` and `trade_id:` from the signed YAML body so
/// the record is filterable without re-parsing the whole intent. Line scan,
/// not a YAML parse — the body is recorded verbatim anyway, so this is only
/// an indexing convenience and a malformed body just yields None.
pub fn ids_from_body(body: &str) -> (Option<String>, Option<String>) {
    let mut id = None;
    let mut trade_id = None;
    for line in body.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("id:") {
            if id.is_none() {
                id = Some(unquote(v));
            }
        } else if let Some(v) = t.strip_prefix("trade_id:") {
            trade_id = Some(unquote(v));
        }
    }
    (id, trade_id)
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches(['"', '\'']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_parsed_from_body() {
        let body =
            "action: enter\nid: ihs-eur-usd-abc123\ntrade_id: ihs-eur-usd\ninstrument: EUR_USD\n";
        let (id, trade_id) = ids_from_body(body);
        assert_eq!(id.as_deref(), Some("ihs-eur-usd-abc123"));
        assert_eq!(trade_id.as_deref(), Some("ihs-eur-usd"));
    }

    #[test]
    fn ids_handle_quotes_and_absence() {
        let (id, trade_id) = ids_from_body("id: \"q-1\"\n");
        assert_eq!(id.as_deref(), Some("q-1"));
        assert_eq!(trade_id, None);
    }

    #[test]
    fn request_id_is_stable_and_varies() {
        let h = vec![("x".to_string(), "y".to_string())];
        let a = mint_request_id("body-a", &h);
        let b = mint_request_id("body-b", &h);
        assert_ne!(a, b, "different bodies → different ids");
        assert_eq!(a, mint_request_id("body-a", &h), "same input → same id");
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn r2_key_is_date_partitioned() {
        let rec = RequestRecord {
            ts: "2026-06-15T07:51:39.123Z".into(),
            request_id: "abc123".into(),
            method: "POST".into(),
            path: "/".into(),
            headers: vec![],
            body: String::new(),
            intent_id: None,
            trade_id: None,
            status: 200,
            outcome: "ok".into(),
            logs: vec![],
        };
        assert_eq!(
            rec.r2_key(),
            "req/2026-06-15/2026-06-15T07:51:39.123Z-abc123.json"
        );
    }

    #[test]
    fn record_round_trips_through_json() {
        // The read side (`plan timeline`) deserializes what the write side
        // serialized — this guards the `Deserialize` derive + `Cow` level field.
        let rec = RequestRecord {
            ts: "2026-06-15T07:51:39.123Z".into(),
            request_id: "abc123".into(),
            method: "POST".into(),
            path: "/".into(),
            headers: vec![("x-api-key".into(), "k".into())],
            body: "action: enter".into(),
            intent_id: Some("hs-eur-usd-abc".into()),
            trade_id: Some("hs-eur-usd".into()),
            status: 200,
            outcome: "entered".into(),
            logs: vec![
                LogLine {
                    level: "log".into(),
                    msg: "entry placed".into(),
                },
                LogLine {
                    level: "error".into(),
                    msg: "boom".into(),
                },
            ],
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: RequestRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.trade_id.as_deref(), Some("hs-eur-usd"));
        assert_eq!(back.logs.len(), 2);
        assert_eq!(back.logs[0].level, "log");
        assert_eq!(back.logs[1].level, "error");
        assert_eq!(back.logs[1].msg, "boom");
    }
}
