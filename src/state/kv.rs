//! Cloudflare KV-backed `StateStore`.
//!
//! Replay protection (`seen:<id>`) and cooldowns (`cooldown:<instrument>`) are
//! stored as TTL keys whose presence is authoritative. To answer the `status`
//! action — which needs to *list* current entries, something the Workers KV
//! SDK doesn't expose — a parallel JSON-encoded index is maintained at
//! `index:cooldowns` and `index:seen`. Indexes are pruned lazily on read and
//! write; they are "best effort" (concurrent writers can race their RMW) but
//! the TTL keys never lie.

use chrono::{DateTime, Utc};
use trade_control_core::state::{
    CooldownEntry, MIN_TTL_SECONDS, SEEN_INDEX_CAP, SeenEntry, Snapshot, StateError, StateStore,
    prune_expired,
};
use worker::kv::KvStore;

const INDEX_COOLDOWNS_KEY: &str = "index:cooldowns";
const INDEX_SEEN_KEY: &str = "index:seen";

pub struct KvStateStore {
    store: KvStore,
}

impl KvStateStore {
    pub fn new(store: KvStore) -> Self {
        Self { store }
    }

    fn seen_key(id: &str) -> String {
        format!("seen:{id}")
    }

    fn cooldown_key(instrument: &str) -> String {
        format!("cooldown:{instrument}")
    }

    async fn read_cooldown_index(&self) -> Result<Vec<CooldownEntry>, StateError> {
        read_index(&self.store, INDEX_COOLDOWNS_KEY).await
    }

    async fn read_seen_index(&self) -> Result<Vec<SeenEntry>, StateError> {
        read_index(&self.store, INDEX_SEEN_KEY).await
    }

    async fn write_cooldown_index(&self, entries: &[CooldownEntry]) -> Result<(), StateError> {
        write_index(&self.store, INDEX_COOLDOWNS_KEY, entries).await
    }

    async fn write_seen_index(&self, entries: &[SeenEntry]) -> Result<(), StateError> {
        write_index(&self.store, INDEX_SEEN_KEY, entries).await
    }
}

async fn read_index<T: for<'de> serde::Deserialize<'de>>(
    store: &KvStore,
    key: &str,
) -> Result<Vec<T>, StateError> {
    let raw = store
        .get(key)
        .text()
        .await
        .map_err(|e| StateError::Backend(format!("get {key}: {e:?}")))?;
    let Some(text) = raw else {
        return Ok(Vec::new());
    };
    serde_json::from_str::<Vec<T>>(&text)
        .map_err(|e| StateError::Backend(format!("decode {key}: {e}")))
}

async fn write_index<T: serde::Serialize>(
    store: &KvStore,
    key: &str,
    entries: &[T],
) -> Result<(), StateError> {
    let body = serde_json::to_string(entries)
        .map_err(|e| StateError::Backend(format!("encode {key}: {e}")))?;
    store
        .put(key, body)
        .map_err(|e| StateError::Backend(format!("put {key} builder: {e:?}")))?
        .execute()
        .await
        .map_err(|e| StateError::Backend(format!("put {key} execute: {e:?}")))
}

impl StateStore for KvStateStore {
    async fn is_seen(&self, id: &str) -> Result<bool, StateError> {
        let key = Self::seen_key(id);
        let result = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get seen: {e:?}")))?;
        Ok(result.is_some())
    }

    async fn mark_seen(&self, id: &str, ttl_seconds: u64) -> Result<(), StateError> {
        let key = Self::seen_key(id);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        self.store
            .put(&key, "1")
            .map_err(|e| StateError::Backend(format!("put seen builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put seen execute: {e:?}")))?;

        // Update the seen index. Best-effort; the TTL key above is the
        // authoritative replay-protection record.
        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(ttl as i64);
        let mut entries = prune_expired(self.read_seen_index().await?, now);
        // Drop any prior entry with the same id, then append.
        entries.retain(|e| e.id != id);
        entries.push(SeenEntry {
            id: id.to_string(),
            expires_at,
        });
        // Cap to the most recent N — keeps the index small.
        if entries.len() > SEEN_INDEX_CAP {
            let drop = entries.len() - SEEN_INDEX_CAP;
            entries.drain(..drop);
        }
        self.write_seen_index(&entries).await
    }

    async fn is_cooled_down(&self, instrument: &str) -> Result<bool, StateError> {
        let key = Self::cooldown_key(instrument);
        let result = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get cooldown: {e:?}")))?;
        Ok(result.is_some())
    }

    async fn set_cooldown(&self, instrument: &str, hours: u32) -> Result<(), StateError> {
        let key = Self::cooldown_key(instrument);
        let ttl = (hours as u64).saturating_mul(3600).max(MIN_TTL_SECONDS);
        self.store
            .put(&key, "1")
            .map_err(|e| StateError::Backend(format!("put cooldown builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put cooldown execute: {e:?}")))?;

        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(ttl as i64);
        let mut entries = prune_expired(self.read_cooldown_index().await?, now);
        entries.retain(|e| e.instrument != instrument);
        entries.push(CooldownEntry {
            instrument: instrument.to_string(),
            expires_at,
        });
        self.write_cooldown_index(&entries).await
    }

    async fn clear_cooldown(&self, instrument: &str) -> Result<bool, StateError> {
        let key = Self::cooldown_key(instrument);
        let was = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get cooldown for clear: {e:?}")))?
            .is_some();
        if was {
            self.store
                .delete(&key)
                .await
                .map_err(|e| StateError::Backend(format!("delete cooldown: {e:?}")))?;
        }
        // Always rewrite the index — both to remove this instrument if listed
        // and to drop any other expired entries we observe.
        let now = Utc::now();
        let mut entries = prune_expired(self.read_cooldown_index().await?, now);
        let before = entries.len();
        entries.retain(|e| e.instrument != instrument);
        if entries.len() != before || was {
            self.write_cooldown_index(&entries).await?;
        }
        Ok(was)
    }

    async fn snapshot(&self) -> Result<Snapshot, StateError> {
        let now: DateTime<Utc> = Utc::now();
        let cooldowns = prune_expired(self.read_cooldown_index().await?, now);
        let recent_seen = prune_expired(self.read_seen_index().await?, now);
        Ok(Snapshot {
            now,
            cooldowns,
            recent_seen,
            preps: Vec::new(),
            vetos: Vec::new(),
        })
    }
}
