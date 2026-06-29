//! In-memory implementations of [`MetadataStore`] and
//! [`CredentialsResolver`] for unit tests and for CLI offline-preview
//! flows.
//!
//! Single-threaded (`RefCell`) — matches the worker runtime model and
//! keeps these `?Send`.

use std::cell::RefCell;
use std::collections::HashMap;

use super::creds::{Credentials, CredentialsError, CredentialsResolver};
use super::metadata::{AccountMetadata, MetadataError, MetadataStore};

/// In-memory metadata store. `Default` produces an empty index.
#[derive(Default)]
pub struct MemMetadataStore {
    inner: RefCell<HashMap<String, AccountMetadata>>,
}

impl MemMetadataStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the store with a record, bypassing the AlreadyExists check.
    /// Test helper — production code must go through `add`.
    pub fn seed(&self, metadata: AccountMetadata) {
        self.inner
            .borrow_mut()
            .insert(metadata.name.clone(), metadata);
    }
}

impl MetadataStore for MemMetadataStore {
    async fn list(&self) -> Result<Vec<AccountMetadata>, MetadataError> {
        let mut out: Vec<AccountMetadata> = self.inner.borrow().values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn get(&self, name: &str) -> Result<AccountMetadata, MetadataError> {
        self.inner
            .borrow()
            .get(name)
            .cloned()
            .ok_or_else(|| MetadataError::NotFound(name.to_owned()))
    }

    async fn add(&self, metadata: AccountMetadata) -> Result<(), MetadataError> {
        let mut inner = self.inner.borrow_mut();
        if inner.contains_key(&metadata.name) {
            return Err(MetadataError::AlreadyExists(metadata.name));
        }
        inner.insert(metadata.name.clone(), metadata);
        Ok(())
    }

    async fn remove(&self, name: &str) -> Result<(), MetadataError> {
        if self.inner.borrow_mut().remove(name).is_some() {
            Ok(())
        } else {
            Err(MetadataError::NotFound(name.to_owned()))
        }
    }
}

/// In-memory credentials resolver. `Default` produces an empty map.
#[derive(Default)]
pub struct MemCredentialsResolver {
    inner: RefCell<HashMap<String, Credentials>>,
}

impl MemCredentialsResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed credentials for `account_name`.
    pub fn seed(&self, account_name: impl Into<String>, creds: Credentials) {
        self.inner.borrow_mut().insert(account_name.into(), creds);
    }
}

impl CredentialsResolver for MemCredentialsResolver {
    async fn resolve(&self, account_name: &str) -> Result<Credentials, CredentialsError> {
        self.inner
            .borrow()
            .get(account_name)
            .cloned()
            .ok_or_else(|| CredentialsError::NotFound(account_name.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{AccountKind, OandaCreds, TradeNationCreds, TradeNationKind};
    use crate::intent::BrokerKind;

    fn meta(name: &str) -> AccountMetadata {
        AccountMetadata::new(name, BrokerKind::TradeNation, AccountKind::Demo)
    }

    #[test]
    fn add_then_get_round_trip() {
        let store = MemMetadataStore::new();
        let m = meta("demo-a");
        pollster::block_on(store.add(m.clone())).unwrap();
        let got = pollster::block_on(store.get("demo-a")).unwrap();
        assert_eq!(got, m);
    }

    #[test]
    fn add_duplicate_errors() {
        let store = MemMetadataStore::new();
        pollster::block_on(store.add(meta("demo-a"))).unwrap();
        let err = pollster::block_on(store.add(meta("demo-a"))).unwrap_err();
        assert!(matches!(err, MetadataError::AlreadyExists(name) if name == "demo-a"));
    }

    #[test]
    fn get_missing_errors_with_name() {
        let store = MemMetadataStore::new();
        let err = pollster::block_on(store.get("ghost")).unwrap_err();
        assert!(matches!(err, MetadataError::NotFound(name) if name == "ghost"));
    }

    #[test]
    fn remove_returns_not_found_for_unknown() {
        let store = MemMetadataStore::new();
        let err = pollster::block_on(store.remove("ghost")).unwrap_err();
        assert!(matches!(err, MetadataError::NotFound(name) if name == "ghost"));
    }

    #[test]
    fn remove_existing_succeeds_and_drops_record() {
        let store = MemMetadataStore::new();
        pollster::block_on(store.add(meta("demo-a"))).unwrap();
        pollster::block_on(store.remove("demo-a")).unwrap();
        let err = pollster::block_on(store.get("demo-a")).unwrap_err();
        assert!(matches!(err, MetadataError::NotFound(_)));
    }

    #[test]
    fn list_returns_sorted_by_name() {
        // Stable ordering matters: the CLI's `account list` output is
        // read by humans, and unstable order would make diffs noisy.
        let store = MemMetadataStore::new();
        for n in ["demo-b", "live-a", "demo-a"] {
            pollster::block_on(store.add(meta(n))).unwrap();
        }
        let listed = pollster::block_on(store.list()).unwrap();
        let names: Vec<&str> = listed.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["demo-a", "demo-b", "live-a"]);
    }

    #[test]
    fn list_on_empty_store_is_empty() {
        let store = MemMetadataStore::new();
        let listed = pollster::block_on(store.list()).unwrap();
        assert!(listed.is_empty());
    }

    #[test]
    fn seed_bypasses_already_exists_check() {
        let store = MemMetadataStore::new();
        store.seed(meta("demo-a"));
        // Even though `seed` already inserted, `add` of a different
        // record with the same name still errors. (`seed` is only for
        // tests; production must go through `add`.)
        let err = pollster::block_on(store.add(meta("demo-a"))).unwrap_err();
        assert!(matches!(err, MetadataError::AlreadyExists(_)));
    }

    #[test]
    fn creds_resolve_round_trip_tradenation() {
        let resolver = MemCredentialsResolver::new();
        let creds = Credentials::TradeNation(TradeNationCreds {
            kind: TradeNationKind::Demo,
            username: "u".into(),
            password: "p".into(),
        });
        resolver.seed("demo-a", creds.clone());
        let got = pollster::block_on(resolver.resolve("demo-a")).unwrap();
        assert_eq!(got, creds);
    }

    #[test]
    fn creds_resolve_round_trip_oanda() {
        let resolver = MemCredentialsResolver::new();
        let creds = Credentials::Oanda(OandaCreds {
            api_key: "k".into(),
            account_id: "id".into(),
        });
        resolver.seed("oanda-1", creds.clone());
        let got = pollster::block_on(resolver.resolve("oanda-1")).unwrap();
        assert_eq!(got, creds);
    }

    #[test]
    fn creds_resolve_missing_errors_with_name() {
        let resolver = MemCredentialsResolver::new();
        let err = pollster::block_on(resolver.resolve("nope")).unwrap_err();
        assert!(matches!(err, CredentialsError::NotFound(name) if name == "nope"));
    }

    /// Run the cross-backend metadata conformance harness against the
    /// reference in-memory store. The native `PgMetadataStore` runs the *same*
    /// `run_all` in `worker/tests/` — keeping the two from drifting.
    #[test]
    fn metadata_conformance_against_memstore() {
        use crate::account::metadata_conformance;
        let store = MemMetadataStore::new();
        pollster::block_on(metadata_conformance::run_all(&store, "mem"));
    }
}
