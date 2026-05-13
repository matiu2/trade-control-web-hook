//! Cloudflare KV-backed `StateStore`.

use worker::kv::KvStore;

use super::{MIN_TTL_SECONDS, StateError, StateStore};

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
            .map_err(|e| StateError::Backend(format!("put seen execute: {e:?}")))
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
            .map_err(|e| StateError::Backend(format!("put cooldown execute: {e:?}")))
    }
}
