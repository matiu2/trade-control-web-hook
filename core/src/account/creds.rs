//! Credential payloads (one variant per broker) and the async resolver
//! trait used to look them up at request time.
//!
//! Credentials are deliberately separated from [`super::AccountMetadata`]
//! so the metadata index can live in KV (no secrets) while the
//! credentials live behind a `CredentialsResolver` impl that reads from
//! Cloudflare Secret Store.

use core::future::Future;

use serde::{Deserialize, Serialize};

/// Credentials for a TradeNation account. Demo and live use the same
/// shape (username + password) but route through different login flows;
/// the `kind` discriminator picks which.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TradeNationCreds {
    pub kind: TradeNationKind,
    pub username: String,
    pub password: String,
}

/// Login flavour for [`TradeNationCreds`]. Mirrors the upstream
/// `AccountType` distinction without dragging the upstream crate into
/// `core/` (this crate must stay wasm-friendly and dependency-light).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TradeNationKind {
    Demo,
    Live,
}

/// Credentials for an OANDA account. Bearer-token auth, so there's no
/// password — the account-id identifies which sub-account of the API
/// key to trade against.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct OandaCreds {
    pub api_key: String,
    pub account_id: String,
}

/// Tagged credential payload. The tag matches the account's
/// `broker` field in metadata; the worker enforces consistency at the
/// boundary so a TradeNation account never resolves OANDA creds.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "broker", rename_all = "lowercase")]
pub enum Credentials {
    TradeNation(TradeNationCreds),
    Oanda(OandaCreds),
}

/// Async resolver for an account's credentials.
///
/// The production impl is wasm-only and reads from `env.secret(...)`
/// inside the Cloudflare Worker. The in-memory impl is for unit tests
/// and the CLI's offline preview mode.
///
/// `?Send` futures match the rest of the codebase — the worker runtime
/// is single-threaded WASM.
pub trait CredentialsResolver {
    fn resolve(
        &self,
        account_name: &str,
    ) -> impl Future<Output = Result<Credentials, CredentialsError>>;
}

/// Failure modes for [`CredentialsResolver::resolve`].
#[derive(Debug)]
pub enum CredentialsError {
    /// No secret binding exists for the named account.
    NotFound(String),
    /// The secret was found but couldn't be parsed as a `Credentials`
    /// payload — most likely a malformed JSON blob.
    Malformed { account: String, reason: String },
    /// Backend (Secret Store / file / etc.) returned an error reading
    /// the secret.
    Backend(String),
}

impl core::fmt::Display for CredentialsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotFound(name) => write!(f, "no credentials secret for account '{name}'"),
            Self::Malformed { account, reason } => {
                write!(f, "credentials secret for '{account}' malformed: {reason}")
            }
            Self::Backend(msg) => write!(f, "credentials backend error: {msg}"),
        }
    }
}

impl std::error::Error for CredentialsError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tradenation_demo_creds_serialise_with_tag() {
        let creds = Credentials::TradeNation(TradeNationCreds {
            kind: TradeNationKind::Demo,
            username: "ABC123".into(),
            password: "shh".into(),
        });
        let json = serde_json::to_string(&creds).unwrap();
        // The `broker` tag is the discriminator.
        assert!(json.contains(r#""broker":"tradenation""#));
        assert!(json.contains(r#""kind":"demo""#));
    }

    #[test]
    fn oanda_creds_serialise_with_tag() {
        let creds = Credentials::Oanda(OandaCreds {
            api_key: "tok".into(),
            account_id: "001-001".into(),
        });
        let json = serde_json::to_string(&creds).unwrap();
        assert!(json.contains(r#""broker":"oanda""#));
        assert!(json.contains(r#""api_key":"tok""#));
    }

    #[test]
    fn round_trip_tradenation_live() {
        let creds = Credentials::TradeNation(TradeNationCreds {
            kind: TradeNationKind::Live,
            username: "user".into(),
            password: "pw".into(),
        });
        let json = serde_json::to_string(&creds).unwrap();
        let back: Credentials = serde_json::from_str(&json).unwrap();
        assert_eq!(back, creds);
    }

    #[test]
    fn round_trip_oanda() {
        let creds = Credentials::Oanda(OandaCreds {
            api_key: "k".into(),
            account_id: "id".into(),
        });
        let json = serde_json::to_string(&creds).unwrap();
        let back: Credentials = serde_json::from_str(&json).unwrap();
        assert_eq!(back, creds);
    }

    #[test]
    fn malformed_creds_error_displays_account() {
        let err = CredentialsError::Malformed {
            account: "live-prod".into(),
            reason: "missing field `password`".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("live-prod"));
        assert!(msg.contains("missing field"));
    }

    #[test]
    fn not_found_error_displays_account() {
        let err = CredentialsError::NotFound("demo-test".into());
        assert!(err.to_string().contains("demo-test"));
    }
}
