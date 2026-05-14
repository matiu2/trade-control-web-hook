//! Storage abstraction for replay protection (`seen:<id>`) and instrument
//! cooldowns (`cooldown:<instrument>`).
//!
//! A trait keeps the dispatch logic transport-agnostic so a non-CF deployment
//! (e.g. self-hosted on a home machine) can swap in a file-backed store later
//! without touching the core. The Cloudflare KV implementation lives next to
//! the trait for now; when a second backend lands it'll move behind a feature.

use std::future::Future;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One active cooldown row in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CooldownEntry {
    pub instrument: String,
    pub expires_at: DateTime<Utc>,
}

/// One recently-seen replay-protection id in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenEntry {
    pub id: String,
    pub expires_at: DateTime<Utc>,
}

/// Read-only snapshot of the state store for the `status` action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub now: DateTime<Utc>,
    pub cooldowns: Vec<CooldownEntry>,
    pub recent_seen: Vec<SeenEntry>,
}

/// Async storage interface. Implementations are `?Send` because the CF Worker
/// runtime is single-threaded WASM and its KV handle is `!Send`.
pub trait StateStore {
    /// Returns true if `id` has already been recorded as seen.
    fn is_seen(&self, id: &str) -> impl Future<Output = Result<bool, StateError>>;

    /// Mark `id` as seen with a TTL in seconds.
    fn mark_seen(&self, id: &str, ttl_seconds: u64)
    -> impl Future<Output = Result<(), StateError>>;

    /// Returns true if `instrument` is currently under cooldown.
    fn is_cooled_down(&self, instrument: &str) -> impl Future<Output = Result<bool, StateError>>;

    /// Set a cooldown on `instrument` for `hours`.
    fn set_cooldown(
        &self,
        instrument: &str,
        hours: u32,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Clear the cooldown for `instrument`. Returns whether it was set before.
    fn clear_cooldown(&self, instrument: &str) -> impl Future<Output = Result<bool, StateError>>;

    /// Return a snapshot of active cooldowns and recent seen ids.
    fn snapshot(&self) -> impl Future<Output = Result<Snapshot, StateError>>;
}

/// Maximum number of recent seen ids retained in the index. Tuning knob;
/// the underlying TTL keys are still authoritative for replay protection.
pub const SEEN_INDEX_CAP: usize = 50;

/// Drop entries whose `expires_at` is at or before `now`. Used by both the
/// cooldown and seen indexes; generic over the entry type so the same pure
/// helper covers both.
pub fn prune_expired<T: HasExpiry>(entries: Vec<T>, now: DateTime<Utc>) -> Vec<T> {
    entries
        .into_iter()
        .filter(|e| e.expires_at() > now)
        .collect()
}

/// Trait for index entries that carry an expiry timestamp. Implemented for
/// [`CooldownEntry`] and [`SeenEntry`] so `prune_expired` can serve both.
pub trait HasExpiry {
    fn expires_at(&self) -> DateTime<Utc>;
}

impl HasExpiry for CooldownEntry {
    fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

impl HasExpiry for SeenEntry {
    fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

#[derive(Debug)]
pub enum StateError {
    Backend(String),
}

impl core::fmt::Display for StateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Backend(msg) => write!(f, "state backend error: {msg}"),
        }
    }
}

impl std::error::Error for StateError {}

/// Cloudflare KV's minimum TTL is 60 seconds; clamp anything smaller.
pub const MIN_TTL_SECONDS: u64 = 60;

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn prune_expired_drops_past_entries() {
        let now = ts("2026-05-14T12:00:00Z");
        let entries = vec![
            CooldownEntry {
                instrument: "EUR_USD".into(),
                expires_at: ts("2026-05-14T11:00:00Z"), // expired
            },
            CooldownEntry {
                instrument: "USD_JPY".into(),
                expires_at: ts("2026-05-14T13:00:00Z"), // live
            },
        ];
        let kept = prune_expired(entries, now);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].instrument, "USD_JPY");
    }

    #[test]
    fn prune_expired_drops_exactly_at_now() {
        let now = ts("2026-05-14T12:00:00Z");
        let entries = vec![SeenEntry {
            id: "edge".into(),
            expires_at: now, // exactly now counts as expired
        }];
        assert!(prune_expired(entries, now).is_empty());
    }

    #[test]
    fn prune_expired_keeps_all_future() {
        let now = ts("2026-05-14T12:00:00Z");
        let entries = vec![
            SeenEntry {
                id: "a".into(),
                expires_at: ts("2026-05-14T13:00:00Z"),
            },
            SeenEntry {
                id: "b".into(),
                expires_at: ts("2026-05-14T14:00:00Z"),
            },
        ];
        assert_eq!(prune_expired(entries, now).len(), 2);
    }

    #[test]
    fn prune_expired_drops_all_past() {
        let now = ts("2026-05-14T12:00:00Z");
        let entries = vec![
            CooldownEntry {
                instrument: "A".into(),
                expires_at: ts("2026-05-13T12:00:00Z"),
            },
            CooldownEntry {
                instrument: "B".into(),
                expires_at: ts("2026-05-13T11:00:00Z"),
            },
        ];
        assert!(prune_expired(entries, now).is_empty());
    }
}
