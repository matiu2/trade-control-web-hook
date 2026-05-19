//! Secret-Store-backed credentials resolver.
//!
//! Maps an account name to the secret binding that holds its
//! credentials, fetches it, and deserialises a [`Credentials`] payload.
//!
//! The binding name follows a stable schema:
//!
//! - `TN_ACCOUNT_<NAME>` for TradeNation accounts
//! - `OANDA_ACCOUNT_<NAME>` for OANDA accounts
//!
//! `<NAME>` is the account name uppercased with `-` mapped to `_`.
//! Wrangler secret names are constrained to `[A-Za-z0-9_]`, so dashes
//! in the operator-friendly name can't survive on the binding side.
//!
//! The blob stored in the secret is the JSON serialisation of a
//! [`Credentials`] value — the tagged enum from `core::account::creds`.

use trade_control_core::account::{Credentials, CredentialsError};
use trade_control_core::intent::BrokerKind;

/// Convert an account name to its secret-binding name. `BrokerKind`
/// picks the prefix (`TN_ACCOUNT_<...>` vs `OANDA_ACCOUNT_<...>`).
///
/// The metadata index is authoritative on which broker an account
/// belongs to, so callers always know which prefix to ask for without
/// having to probe both.
///
/// Used by the wasm resolver; kept available unconditionally so the
/// native tests can cover the naming rules.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn secret_name_for(broker: BrokerKind, account_name: &str) -> String {
    let prefix = match broker {
        BrokerKind::TradeNation => "TN_ACCOUNT_",
        BrokerKind::Oanda => "OANDA_ACCOUNT_",
    };
    let normalised = account_name.to_ascii_uppercase().replace('-', "_");
    format!("{prefix}{normalised}")
}

/// Parse a secret blob as `Credentials`. Pure — testable without a
/// wasm runtime. The wasm resolver below is a thin wrapper that fetches
/// the secret then defers to this.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn parse_credentials_blob(account: &str, raw: &str) -> Result<Credentials, CredentialsError> {
    serde_json::from_str::<Credentials>(raw).map_err(|e| CredentialsError::Malformed {
        account: account.to_owned(),
        reason: e.to_string(),
    })
}

#[cfg(target_arch = "wasm32")]
pub use wasm::SecretCredentialsResolver;

#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;
    use trade_control_core::account::{CredentialsResolver, MetadataStore};
    use worker::Env;

    /// Wasm-only `CredentialsResolver` that reads from Worker secret
    /// bindings.
    ///
    /// Holds a reference to `Env` for the duration of one request — the
    /// lifetime parameter ties this to the request scope so we never
    /// carry the env across async boundaries it isn't valid in.
    pub struct SecretCredentialsResolver<'a, M: MetadataStore> {
        env: &'a Env,
        metadata: &'a M,
    }

    impl<'a, M: MetadataStore> SecretCredentialsResolver<'a, M> {
        /// Construct a resolver. The metadata store is needed because
        /// secret-binding name depends on the account's broker, which
        /// the resolver doesn't otherwise know.
        pub fn new(env: &'a Env, metadata: &'a M) -> Self {
            Self { env, metadata }
        }
    }

    impl<M: MetadataStore> CredentialsResolver for SecretCredentialsResolver<'_, M> {
        async fn resolve(&self, account_name: &str) -> Result<Credentials, CredentialsError> {
            // Need the broker to know which prefix to look up.
            let meta = self.metadata.get(account_name).await.map_err(|e| {
                CredentialsError::Backend(format!("metadata lookup for {account_name}: {e}"))
            })?;
            let binding = secret_name_for(meta.broker, account_name);
            let raw = self
                .env
                .secret(&binding)
                .map_err(|_| CredentialsError::NotFound(account_name.to_owned()))?
                .to_string();
            parse_credentials_blob(account_name, &raw)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_core::account::{OandaCreds, TradeNationCreds, TradeNationKind};

    #[test]
    fn tradenation_secret_name_uppercases_and_swaps_dashes() {
        let name = secret_name_for(BrokerKind::TradeNation, "demo-alice");
        assert_eq!(name, "TN_ACCOUNT_DEMO_ALICE");
    }

    #[test]
    fn oanda_secret_name_uses_correct_prefix() {
        let name = secret_name_for(BrokerKind::Oanda, "live-prod");
        assert_eq!(name, "OANDA_ACCOUNT_LIVE_PROD");
    }

    #[test]
    fn secret_name_leaves_alnum_intact() {
        // No-op transform for an already-safe name. Confirms we don't
        // mangle names like `demo1` that don't need normalising.
        let name = secret_name_for(BrokerKind::TradeNation, "demo1");
        assert_eq!(name, "TN_ACCOUNT_DEMO1");
    }

    #[test]
    fn parse_blob_round_trip_tradenation() {
        let creds = Credentials::TradeNation(TradeNationCreds {
            kind: TradeNationKind::Demo,
            username: "u".into(),
            password: "p".into(),
        });
        let raw = serde_json::to_string(&creds).unwrap();
        let back = parse_credentials_blob("demo-a", &raw).unwrap();
        assert_eq!(back, creds);
    }

    #[test]
    fn parse_blob_round_trip_oanda() {
        let creds = Credentials::Oanda(OandaCreds {
            api_key: "k".into(),
            account_id: "id".into(),
        });
        let raw = serde_json::to_string(&creds).unwrap();
        let back = parse_credentials_blob("oanda-1", &raw).unwrap();
        assert_eq!(back, creds);
    }

    #[test]
    fn parse_blob_malformed_surfaces_account_name() {
        // The operator who set the bad secret needs to know which
        // account they botched.
        let err = parse_credentials_blob("live-prod", "not json").unwrap_err();
        match err {
            CredentialsError::Malformed { account, reason } => {
                assert_eq!(account, "live-prod");
                assert!(!reason.is_empty());
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn parse_blob_wrong_broker_tag_surfaces_malformed() {
        // The JSON parses but the tag is unknown — surfaces as
        // Malformed (not BrokerMismatch; that check belongs in
        // AccountStore::resolve, after both halves are loaded).
        let err = parse_credentials_blob("x", r#"{"broker":"binance"}"#).unwrap_err();
        assert!(matches!(err, CredentialsError::Malformed { .. }));
    }
}
