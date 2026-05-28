//! Per-account session-freshness metadata, stored in KV alongside the
//! cached `tradenation_api::Session` JSON. Keeping this separate keeps
//! the cached session wire-compatible with `broker_tradenation::login`
//! (which deserialises the bare upstream `Session` struct).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Sidecar record for a cached TN session. The cron pre-warm reads
/// `cached_at` to decide whether the session has gone stale.
///
/// `dead_code` is allowed because both this struct and [`key`] are only
/// constructed from `#[cfg(target_arch = "wasm32")]` call sites plus
/// tests — native builds see no live consumer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct SessionMeta {
    pub cached_at: DateTime<Utc>,
}

/// KV key for the sibling metadata slot. Lives next to
/// `tn:session:{account}` (see `tn_session_cache_key` in `lib.rs`).
#[allow(dead_code)]
pub fn key(account: &str) -> String {
    format!("tn:session_meta:{account}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn key_format_matches_session_sibling() {
        assert_eq!(key("demo-1"), "tn:session_meta:demo-1");
    }

    #[test]
    fn session_meta_round_trips_through_json() {
        let cached_at = Utc.with_ymd_and_hms(2026, 5, 28, 10, 30, 0).unwrap();
        let meta = SessionMeta { cached_at };
        let json = serde_json::to_string(&meta).unwrap();
        let back: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cached_at, cached_at);
    }
}
