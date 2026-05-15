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
    CooldownEntry, MIN_TTL_SECONDS, PREP_INDEX_CAP, PrepEntry, SEEN_INDEX_CAP, SeenEntry, Snapshot,
    StateError, StateStore, VETO_INDEX_CAP, VetoEntry, prune_expired,
};
use worker::kv::KvStore;

const INDEX_COOLDOWNS_KEY: &str = "index:cooldowns";
const INDEX_SEEN_KEY: &str = "index:seen";
const INDEX_PREPS_KEY: &str = "index:preps";
const INDEX_VETOS_KEY: &str = "index:vetos";

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

    fn prep_key(instrument: &str, step: &str) -> String {
        format!("prep:{instrument}:{step}")
    }

    fn veto_key(instrument: &str, name: &str) -> String {
        format!("veto:{instrument}:{name}")
    }

    async fn read_cooldown_index(&self) -> Result<Vec<CooldownEntry>, StateError> {
        read_index(&self.store, INDEX_COOLDOWNS_KEY).await
    }

    async fn read_seen_index(&self) -> Result<Vec<SeenEntry>, StateError> {
        read_index(&self.store, INDEX_SEEN_KEY).await
    }

    async fn read_prep_index(&self) -> Result<Vec<PrepEntry>, StateError> {
        read_index(&self.store, INDEX_PREPS_KEY).await
    }

    async fn read_veto_index(&self) -> Result<Vec<VetoEntry>, StateError> {
        read_index(&self.store, INDEX_VETOS_KEY).await
    }

    async fn write_cooldown_index(&self, entries: &[CooldownEntry]) -> Result<(), StateError> {
        write_index(&self.store, INDEX_COOLDOWNS_KEY, entries).await
    }

    async fn write_seen_index(&self, entries: &[SeenEntry]) -> Result<(), StateError> {
        write_index(&self.store, INDEX_SEEN_KEY, entries).await
    }

    async fn write_prep_index(&self, entries: &[PrepEntry]) -> Result<(), StateError> {
        write_index(&self.store, INDEX_PREPS_KEY, entries).await
    }

    async fn write_veto_index(&self, entries: &[VetoEntry]) -> Result<(), StateError> {
        write_index(&self.store, INDEX_VETOS_KEY, entries).await
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

    async fn set_prep(
        &self,
        instrument: &str,
        step: &str,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::prep_key(instrument, step);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        // Store the set_at timestamp as the value so the entry-time gate
        // can enforce prep ordering. RFC3339 round-trips through chrono.
        let body = now.to_rfc3339();
        self.store
            .put(&key, body)
            .map_err(|e| StateError::Backend(format!("put prep builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put prep execute: {e:?}")))?;

        let expires_at = now + chrono::Duration::seconds(ttl as i64);
        let mut entries = prune_expired(self.read_prep_index().await?, now);
        entries.retain(|e| !(e.instrument == instrument && e.step == step));
        entries.push(PrepEntry {
            instrument: instrument.to_string(),
            step: step.to_string(),
            set_at: now,
            expires_at,
        });
        if entries.len() > PREP_INDEX_CAP {
            let drop = entries.len() - PREP_INDEX_CAP;
            entries.drain(..drop);
        }
        self.write_prep_index(&entries).await
    }

    async fn get_prep(
        &self,
        instrument: &str,
        step: &str,
    ) -> Result<Option<DateTime<Utc>>, StateError> {
        let key = Self::prep_key(instrument, step);
        let raw = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get prep: {e:?}")))?;
        let Some(text) = raw else {
            return Ok(None);
        };
        let ts = DateTime::parse_from_rfc3339(&text)
            .map_err(|e| StateError::Backend(format!("parse prep timestamp: {e}")))?
            .with_timezone(&Utc);
        Ok(Some(ts))
    }

    async fn clear_prep(&self, instrument: &str, step: &str) -> Result<bool, StateError> {
        let key = Self::prep_key(instrument, step);
        let was = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get prep for clear: {e:?}")))?
            .is_some();
        if was {
            self.store
                .delete(&key)
                .await
                .map_err(|e| StateError::Backend(format!("delete prep: {e:?}")))?;
        }
        let now = Utc::now();
        let mut entries = prune_expired(self.read_prep_index().await?, now);
        let before = entries.len();
        entries.retain(|e| !(e.instrument == instrument && e.step == step));
        if entries.len() != before || was {
            self.write_prep_index(&entries).await?;
        }
        Ok(was)
    }

    async fn set_veto(
        &self,
        instrument: &str,
        name: &str,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::veto_key(instrument, name);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        self.store
            .put(&key, "1")
            .map_err(|e| StateError::Backend(format!("put veto builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put veto execute: {e:?}")))?;

        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(ttl as i64);
        let mut entries = prune_expired(self.read_veto_index().await?, now);
        entries.retain(|e| !(e.instrument == instrument && e.name == name));
        entries.push(VetoEntry {
            instrument: instrument.to_string(),
            name: name.to_string(),
            expires_at,
        });
        if entries.len() > VETO_INDEX_CAP {
            let drop = entries.len() - VETO_INDEX_CAP;
            entries.drain(..drop);
        }
        self.write_veto_index(&entries).await
    }

    async fn is_vetoed(&self, instrument: &str, name: &str) -> Result<bool, StateError> {
        let key = Self::veto_key(instrument, name);
        let result = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get veto: {e:?}")))?;
        Ok(result.is_some())
    }

    async fn clear_veto(&self, instrument: &str, name: &str) -> Result<bool, StateError> {
        let key = Self::veto_key(instrument, name);
        let was = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get veto for clear: {e:?}")))?
            .is_some();
        if was {
            self.store
                .delete(&key)
                .await
                .map_err(|e| StateError::Backend(format!("delete veto: {e:?}")))?;
        }
        let now = Utc::now();
        let mut entries = prune_expired(self.read_veto_index().await?, now);
        let before = entries.len();
        entries.retain(|e| !(e.instrument == instrument && e.name == name));
        if entries.len() != before || was {
            self.write_veto_index(&entries).await?;
        }
        Ok(was)
    }

    async fn snapshot(&self) -> Result<Snapshot, StateError> {
        let now: DateTime<Utc> = Utc::now();
        let cooldowns = prune_expired(self.read_cooldown_index().await?, now);
        let recent_seen = prune_expired(self.read_seen_index().await?, now);
        let preps = prune_expired(self.read_prep_index().await?, now);
        let vetos = prune_expired(self.read_veto_index().await?, now);
        Ok(Snapshot {
            now,
            cooldowns,
            recent_seen,
            preps,
            vetos,
        })
    }
}
