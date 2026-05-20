//! Account metadata: the non-secret record describing what an account
//! is for. Stored in the index alongside the credential secret binding.

use core::future::Future;

use serde::{Deserialize, Serialize};

use crate::intent::BrokerKind;

use super::caps::AccountCaps;
use super::kind::AccountKind;

/// Non-secret description of an account. Lives in the metadata store;
/// the credentials themselves live behind
/// [`super::CredentialsResolver`].
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct AccountMetadata {
    /// Stable name used as the key in the index and as the suffix on
    /// the credential secret binding. Operator-chosen; should be
    /// `kebab-case` and unique within the worker.
    pub name: String,
    /// Which broker this account belongs to.
    pub broker: BrokerKind,
    /// Demo or live.
    pub kind: AccountKind,
    /// Optional per-account risk caps. See [`AccountCaps`] for the
    /// "narrower wins" semantics.
    #[serde(default, skip_serializing_if = "is_default_caps")]
    pub caps: AccountCaps,
}

fn is_default_caps(caps: &AccountCaps) -> bool {
    caps == &AccountCaps::default()
}

impl AccountMetadata {
    /// Construct a fresh metadata record with default (worker-wide) caps.
    pub fn new(name: impl Into<String>, broker: BrokerKind, kind: AccountKind) -> Self {
        Self {
            name: name.into(),
            broker,
            kind,
            caps: AccountCaps::default(),
        }
    }
}

/// Failure modes for the metadata index. Distinct from
/// [`super::CredentialsError`] so callers can tell "no such account"
/// from "creds missing for known account".
#[derive(Debug)]
pub enum MetadataError {
    NotFound(String),
    AlreadyExists(String),
    Backend(String),
}

impl core::fmt::Display for MetadataError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotFound(name) => write!(f, "no account named '{name}'"),
            Self::AlreadyExists(name) => write!(f, "account '{name}' already exists"),
            Self::Backend(msg) => write!(f, "metadata backend error: {msg}"),
        }
    }
}

impl std::error::Error for MetadataError {}

/// Async CRUD over the account metadata index. `?Send` futures to
/// match the rest of the codebase (single-threaded wasm runtime).
pub trait MetadataStore {
    /// Return every account currently in the index, in stable order
    /// (by name, ascending). Stable order matters because the CLI's
    /// `account list` output is read by humans.
    fn list(&self) -> impl Future<Output = Result<Vec<AccountMetadata>, MetadataError>>;

    /// Fetch one account by name.
    fn get(&self, name: &str) -> impl Future<Output = Result<AccountMetadata, MetadataError>>;

    /// Insert a brand-new account record. Errors with
    /// [`MetadataError::AlreadyExists`] if `name` is taken.
    fn add(&self, metadata: AccountMetadata) -> impl Future<Output = Result<(), MetadataError>>;

    /// Remove an account from the index. Errors with
    /// [`MetadataError::NotFound`] if `name` doesn't exist — callers
    /// can map that to a no-op if they prefer.
    fn remove(&self, name: &str) -> impl Future<Output = Result<(), MetadataError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_metadata_has_default_caps() {
        let m = AccountMetadata::new("demo-1", BrokerKind::TradeNation, AccountKind::Demo);
        assert_eq!(m.name, "demo-1");
        assert_eq!(m.broker, BrokerKind::TradeNation);
        assert_eq!(m.kind, AccountKind::Demo);
        assert_eq!(m.caps, AccountCaps::default());
    }

    #[test]
    fn metadata_yaml_omits_default_caps() {
        // Default caps shouldn't bloat every record on the wire with
        // `caps: {}` — operators reading the YAML by eye care about
        // the present fields.
        let m = AccountMetadata::new("demo-1", BrokerKind::TradeNation, AccountKind::Demo);
        let yaml = serde_yaml::to_string(&m).unwrap();
        assert!(!yaml.contains("caps"));
    }

    #[test]
    fn metadata_yaml_includes_present_caps() {
        let m = AccountMetadata {
            name: "live-prod".into(),
            broker: BrokerKind::TradeNation,
            kind: AccountKind::Live,
            caps: AccountCaps {
                max_risk_pct: Some(0.25),
                max_open_positions: Some(1),
                min_position_size: None,
            },
        };
        let yaml = serde_yaml::to_string(&m).unwrap();
        assert!(yaml.contains("caps:"));
        assert!(yaml.contains("max_risk_pct: 0.25"));
        assert!(yaml.contains("max_open_positions: 1"));
    }

    #[test]
    fn metadata_round_trip_yaml() {
        let m = AccountMetadata {
            name: "live-prod".into(),
            broker: BrokerKind::TradeNation,
            kind: AccountKind::Live,
            caps: AccountCaps {
                max_risk_pct: Some(0.25),
                max_open_positions: Some(1),
                min_position_size: None,
            },
        };
        let yaml = serde_yaml::to_string(&m).unwrap();
        let back: AccountMetadata = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn not_found_error_renders_name() {
        let err = MetadataError::NotFound("nope".into());
        assert!(err.to_string().contains("nope"));
    }
}
