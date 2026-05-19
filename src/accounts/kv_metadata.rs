//! Cloudflare KV implementation of the account metadata index.
//!
//! Everything goes in one key as a JSON-serialised
//! `Vec<AccountMetadata>` — the index is small (max ~20 accounts per
//! deploy, per TODO.md) so a single-key shape avoids the listing-cost
//! of a per-account-key approach. Reads pay one `KV.get`; writes pay
//! one `KV.put`.

use std::cmp::Ordering;

use trade_control_core::account::{AccountMetadata, MetadataError, MetadataStore};
use worker::kv::KvStore;

/// KV key for the entire account metadata index.
pub const ACCOUNT_INDEX_KEY: &str = "accounts:index";

/// KV-backed metadata store.
pub struct KvMetadataStore {
    store: KvStore,
}

impl KvMetadataStore {
    pub fn new(store: KvStore) -> Self {
        Self { store }
    }

    async fn read_all(&self) -> Result<Vec<AccountMetadata>, MetadataError> {
        let raw = self
            .store
            .get(ACCOUNT_INDEX_KEY)
            .text()
            .await
            .map_err(|e| MetadataError::Backend(format!("get index: {e:?}")))?;
        let Some(text) = raw else {
            return Ok(Vec::new());
        };
        serde_json::from_str::<Vec<AccountMetadata>>(&text)
            .map_err(|e| MetadataError::Backend(format!("decode index: {e}")))
    }

    async fn write_all(&self, entries: &[AccountMetadata]) -> Result<(), MetadataError> {
        let body = serde_json::to_string(entries)
            .map_err(|e| MetadataError::Backend(format!("encode index: {e}")))?;
        self.store
            .put(ACCOUNT_INDEX_KEY, body)
            .map_err(|e| MetadataError::Backend(format!("put index builder: {e:?}")))?
            .execute()
            .await
            .map_err(|e| MetadataError::Backend(format!("put index execute: {e:?}")))
    }
}

impl MetadataStore for KvMetadataStore {
    async fn list(&self) -> Result<Vec<AccountMetadata>, MetadataError> {
        let mut all = self.read_all().await?;
        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(all)
    }

    async fn get(&self, name: &str) -> Result<AccountMetadata, MetadataError> {
        let all = self.read_all().await?;
        all.into_iter()
            .find(|m| m.name == name)
            .ok_or_else(|| MetadataError::NotFound(name.to_owned()))
    }

    async fn add(&self, metadata: AccountMetadata) -> Result<(), MetadataError> {
        let mut all = self.read_all().await?;
        if all.iter().any(|m| m.name == metadata.name) {
            return Err(MetadataError::AlreadyExists(metadata.name));
        }
        all.push(metadata);
        all.sort_by(name_then_kind);
        self.write_all(&all).await
    }

    async fn remove(&self, name: &str) -> Result<(), MetadataError> {
        let mut all = self.read_all().await?;
        let before = all.len();
        all.retain(|m| m.name != name);
        if all.len() == before {
            return Err(MetadataError::NotFound(name.to_owned()));
        }
        self.write_all(&all).await
    }
}

/// Stable sort key for the index: by name. Defined as a free function
/// (not a closure) so the trait `add` impl can stay terse and the
/// ordering is uniform with `list`.
fn name_then_kind(a: &AccountMetadata, b: &AccountMetadata) -> Ordering {
    a.name.cmp(&b.name)
}
