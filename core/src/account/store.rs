//! Bundled metadata-store + credentials-resolver.
//!
//! [`AccountStore`] is the surface the worker dispatch and the CLI both
//! talk to. It enforces the broker-vs-creds consistency invariant
//! (TradeNation metadata must resolve TradeNation credentials, etc.)
//! at the boundary so callers downstream can trust the pairing.

use super::creds::{Credentials, CredentialsError, CredentialsResolver};
use super::metadata::{AccountMetadata, MetadataError, MetadataStore};
use crate::intent::BrokerKind;

/// Composite failure type. Distinct from the inner errors so a caller
/// can tell "metadata says no" from "creds say no" from "broker
/// mismatch" without dropping detail.
#[derive(Debug)]
pub enum AccountStoreError {
    Metadata(MetadataError),
    Credentials(CredentialsError),
    /// The metadata declared one broker but the credential payload
    /// belongs to another. Indicates either a misconfigured secret or
    /// a renamed account whose old credentials weren't cleared.
    BrokerMismatch {
        account: String,
        expected: BrokerKind,
        actual: BrokerKind,
    },
}

impl core::fmt::Display for AccountStoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Metadata(e) => write!(f, "{e}"),
            Self::Credentials(e) => write!(f, "{e}"),
            Self::BrokerMismatch {
                account,
                expected,
                actual,
            } => write!(
                f,
                "account '{account}' metadata says broker {expected:?} but credentials are for {actual:?}"
            ),
        }
    }
}

impl std::error::Error for AccountStoreError {}

impl From<MetadataError> for AccountStoreError {
    fn from(e: MetadataError) -> Self {
        Self::Metadata(e)
    }
}

impl From<CredentialsError> for AccountStoreError {
    fn from(e: CredentialsError) -> Self {
        Self::Credentials(e)
    }
}

/// Bundle of a metadata store and a credentials resolver.
///
/// Generic over both halves so the worker can wire (KV, Secret Store)
/// and tests can wire (Mem, Mem). Both halves are `?Send` per the
/// codebase convention.
pub struct AccountStore<M, C> {
    pub metadata: M,
    pub credentials: C,
}

impl<M, C> AccountStore<M, C>
where
    M: MetadataStore,
    C: CredentialsResolver,
{
    pub fn new(metadata: M, credentials: C) -> Self {
        Self {
            metadata,
            credentials,
        }
    }

    /// List every account in the metadata index. Sorted by name.
    pub async fn list(&self) -> Result<Vec<AccountMetadata>, AccountStoreError> {
        Ok(self.metadata.list().await?)
    }

    /// Get one account's metadata. Does not touch credentials.
    pub async fn get(&self, name: &str) -> Result<AccountMetadata, AccountStoreError> {
        Ok(self.metadata.get(name).await?)
    }

    /// Resolve metadata + credentials together, verifying they refer to
    /// the same broker. Use this on the entry path.
    pub async fn resolve(
        &self,
        name: &str,
    ) -> Result<(AccountMetadata, Credentials), AccountStoreError> {
        let meta = self.metadata.get(name).await?;
        let creds = self.credentials.resolve(name).await?;
        let actual = match &creds {
            Credentials::TradeNation(_) => BrokerKind::TradeNation,
            Credentials::Oanda(_) => BrokerKind::Oanda,
        };
        if actual != meta.broker {
            return Err(AccountStoreError::BrokerMismatch {
                account: name.to_owned(),
                expected: meta.broker,
                actual,
            });
        }
        Ok((meta, creds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{
        AccountKind, MemCredentialsResolver, MemMetadataStore, OandaCreds, TradeNationCreds,
        TradeNationKind,
    };

    fn store_with(
        meta: AccountMetadata,
        creds: Credentials,
    ) -> AccountStore<MemMetadataStore, MemCredentialsResolver> {
        let m = MemMetadataStore::new();
        m.seed(meta.clone());
        let c = MemCredentialsResolver::new();
        c.seed(meta.name.clone(), creds);
        AccountStore::new(m, c)
    }

    #[test]
    fn resolve_round_trip_tradenation() {
        let meta = AccountMetadata::new("demo-a", BrokerKind::TradeNation, AccountKind::Demo);
        let creds = Credentials::TradeNation(TradeNationCreds {
            kind: TradeNationKind::Demo,
            username: "u".into(),
            password: "p".into(),
        });
        let store = store_with(meta.clone(), creds.clone());
        let (got_meta, got_creds) = pollster::block_on(store.resolve("demo-a")).unwrap();
        assert_eq!(got_meta, meta);
        assert_eq!(got_creds, creds);
    }

    #[test]
    fn resolve_broker_mismatch_errors() {
        // Metadata claims OANDA, creds say TradeNation. This is the
        // shape of an accidentally-renamed account where the old secret
        // outlived its index entry.
        let meta = AccountMetadata::new("mixed", BrokerKind::Oanda, AccountKind::Demo);
        let creds = Credentials::TradeNation(TradeNationCreds {
            kind: TradeNationKind::Demo,
            username: "u".into(),
            password: "p".into(),
        });
        let store = store_with(meta, creds);
        let err = pollster::block_on(store.resolve("mixed")).unwrap_err();
        match err {
            AccountStoreError::BrokerMismatch {
                account,
                expected,
                actual,
            } => {
                assert_eq!(account, "mixed");
                assert_eq!(expected, BrokerKind::Oanda);
                assert_eq!(actual, BrokerKind::TradeNation);
            }
            other => panic!("expected BrokerMismatch, got {other:?}"),
        }
    }

    #[test]
    fn resolve_missing_metadata_surfaces_not_found() {
        let store = AccountStore::new(MemMetadataStore::new(), MemCredentialsResolver::new());
        let err = pollster::block_on(store.resolve("ghost")).unwrap_err();
        assert!(matches!(
            err,
            AccountStoreError::Metadata(MetadataError::NotFound(name)) if name == "ghost"
        ));
    }

    #[test]
    fn resolve_missing_creds_surfaces_not_found() {
        // Metadata exists but the secret binding wasn't written. The
        // CLI's `account test` should bubble this up so the operator
        // knows to set the secret.
        let m = MemMetadataStore::new();
        m.seed(AccountMetadata::new(
            "demo-a",
            BrokerKind::TradeNation,
            AccountKind::Demo,
        ));
        let store = AccountStore::new(m, MemCredentialsResolver::new());
        let err = pollster::block_on(store.resolve("demo-a")).unwrap_err();
        assert!(matches!(
            err,
            AccountStoreError::Credentials(CredentialsError::NotFound(name)) if name == "demo-a"
        ));
    }

    #[test]
    fn list_returns_sorted_metadata() {
        let m = MemMetadataStore::new();
        for n in ["live-x", "demo-a"] {
            m.seed(AccountMetadata::new(
                n,
                BrokerKind::TradeNation,
                AccountKind::Demo,
            ));
        }
        let store = AccountStore::new(m, MemCredentialsResolver::new());
        let listed = pollster::block_on(store.list()).unwrap();
        let names: Vec<&str> = listed.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["demo-a", "live-x"]);
    }

    #[test]
    fn resolve_oanda_round_trip() {
        let meta = AccountMetadata::new("oanda-1", BrokerKind::Oanda, AccountKind::Demo);
        let creds = Credentials::Oanda(OandaCreds {
            api_key: "k".into(),
            account_id: "id".into(),
        });
        let store = store_with(meta.clone(), creds.clone());
        let (got_meta, got_creds) = pollster::block_on(store.resolve("oanda-1")).unwrap();
        assert_eq!(got_meta, meta);
        assert_eq!(got_creds, creds);
    }
}
