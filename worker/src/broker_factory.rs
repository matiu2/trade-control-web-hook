//! Native broker construction — the VM replacement for the wasm worker's
//! `acquire_oanda_broker` / `acquire_tn_broker` (`src/lib.rs`).
//!
//! The wasm worker built brokers from Cloudflare `Env` secrets + the KV account
//! index + a KV session cache. Natively the inputs are:
//!
//! * **account metadata** — an [`AccountMetadata`] read from the Postgres
//!   account index ([`crate::PgMetadataStore`]): which broker, demo/live, and
//!   (for OANDA) the sub-account id.
//! * **secrets** — [`crate::Secrets`] from the process env (the OANDA token +
//!   global live flag).
//! * **TradeNation credentials** — resolved natively by `tradenation_api` from
//!   the enc account store (`~/.config/tradenation/accounts.enc`) keyed by the
//!   account *name*. The whole wasm redirect-chain login (`src/tn_login.rs`) is
//!   unnecessary off-wasm — `tradenation_api::login_demo_named` does it.
//!
//! Like the wasm worker, this does **not** type-erase the broker (the `Broker`
//! trait is not object-safe — its methods return `impl Future`). The native
//! dispatcher branches on [`BrokerKind`] and calls the matching `acquire_*`,
//! then monomorphizes the generic dispatch per arm — exactly as the wasm
//! worker's `main` does.

use broker_oanda::OandaBroker;
use broker_tradenation_adapter::TradeNationAdapter;
use trade_control_core::account::AccountMetadata;
use trade_control_core::intent::BrokerKind;

use crate::Secrets;

/// Why a broker couldn't be constructed. The dispatcher maps these to the same
/// HTTP statuses the wasm worker used (OANDA login fail → 500, TN login fail →
/// 503) and logs the detail.
#[derive(Debug)]
pub enum BrokerError {
    /// The account's `broker` tag didn't match the broker the caller asked for
    /// (an intent's `broker` must match its named account's recorded broker).
    BrokerMismatch {
        intent: BrokerKind,
        account: BrokerKind,
    },
    /// An OANDA account record is missing its `oanda_account_id` (required to
    /// route to the right sub-account under the shared token).
    MissingOandaAccountId { account: String },
    /// `OANDA_API_KEY` isn't set but an OANDA account was requested.
    MissingOandaApiKey,
    /// TradeNation native login failed (no such account in the enc store, bad
    /// credentials, or a network failure). Carries the underlying message.
    TradeNationLogin(String),
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BrokerMismatch { intent, account } => write!(
                f,
                "intent broker {intent:?} does not match account broker {account:?}"
            ),
            Self::MissingOandaAccountId { account } => {
                write!(f, "oanda account '{account}' has no oanda_account_id")
            }
            Self::MissingOandaApiKey => write!(f, "OANDA_API_KEY not set"),
            Self::TradeNationLogin(msg) => write!(f, "tradenation login failed: {msg}"),
        }
    }
}

impl std::error::Error for BrokerError {}

/// Build an [`OandaBroker`] for `meta`. The token comes from `secrets`; the
/// sub-account id from `meta`; live/practice from the account's `kind` (so each
/// account hits its own OANDA environment regardless of the global flag —
/// matching the wasm worker's per-account behaviour).
pub fn acquire_oanda(
    meta: &AccountMetadata,
    secrets: &Secrets,
) -> Result<OandaBroker, BrokerError> {
    if meta.broker != BrokerKind::Oanda {
        return Err(BrokerError::BrokerMismatch {
            intent: BrokerKind::Oanda,
            account: meta.broker,
        });
    }
    let account_id =
        meta.oanda_account_id
            .clone()
            .ok_or_else(|| BrokerError::MissingOandaAccountId {
                account: meta.name.clone(),
            })?;
    let api_key = secrets
        .oanda_api_key
        .clone()
        .ok_or(BrokerError::MissingOandaApiKey)?;
    Ok(OandaBroker::from_api_key(
        api_key,
        account_id,
        meta.kind.is_live(),
    ))
}

/// Build a [`TradeNationAdapter`] for `meta`, logging in natively against the
/// enc account store keyed by the account *name*. (TradeNation identifies the
/// account by its session credentials, not an id, so `oanda_account_id` is
/// irrelevant here.)
pub async fn acquire_tn(meta: &AccountMetadata) -> Result<TradeNationAdapter, BrokerError> {
    if meta.broker != BrokerKind::TradeNation {
        return Err(BrokerError::BrokerMismatch {
            intent: BrokerKind::TradeNation,
            account: meta.broker,
        });
    }
    // Native login walks the redirect chain and reads the enc store; no Worker
    // Fetch, no KV session cache. A future session cache (Postgres) is an
    // optimisation, not a correctness need — the broker re-logins on rejection.
    let session = tradenation_api::login_demo_named(&meta.name)
        .await
        .map_err(|e| BrokerError::TradeNationLogin(e.to_string()))?;
    // `broker_tradenation::login` takes the session as a JSON string (the wasm
    // worker fed it the KV-cached blob); serialize the fresh session to match.
    let session_json = serde_json::to_string(&session)
        .map_err(|e| BrokerError::TradeNationLogin(format!("serialize session: {e}")))?;
    let broker = broker_tradenation::login(&session_json)
        .await
        .ok_or_else(|| {
            BrokerError::TradeNationLogin("broker_tradenation::login returned None".into())
        })?;
    Ok(TradeNationAdapter(broker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_core::account::AccountKind;

    fn secrets_with_oanda() -> Secrets {
        Secrets {
            signing_key: "sk".into(),
            admin_key: "ak".into(),
            max_risk_pct: 1.0,
            max_open_positions: 3.0,
            oanda_api_key: Some("token".into()),
            oanda_live: false,
        }
    }

    fn oanda_meta(account_id: Option<&str>, kind: AccountKind) -> AccountMetadata {
        AccountMetadata {
            name: "oanda-demo".into(),
            broker: BrokerKind::Oanda,
            kind,
            caps: Default::default(),
            oanda_account_id: account_id.map(str::to_owned),
        }
    }

    // The broker types don't implement `Debug` (they wrap live API clients), so
    // assertions match on the result via `matches!` rather than `unwrap_err`.

    #[test]
    fn oanda_happy_path_builds() {
        let meta = oanda_meta(Some("101-011-1-003"), AccountKind::Demo);
        let result = acquire_oanda(&meta, &secrets_with_oanda());
        assert!(
            result.is_ok(),
            "a complete oanda record must build a broker"
        );
    }

    #[test]
    fn oanda_missing_account_id_errors() {
        let meta = oanda_meta(None, AccountKind::Demo);
        let result = acquire_oanda(&meta, &secrets_with_oanda());
        assert!(matches!(
            result,
            Err(BrokerError::MissingOandaAccountId { .. })
        ));
    }

    #[test]
    fn oanda_missing_api_key_errors() {
        let meta = oanda_meta(Some("101-011-1-003"), AccountKind::Demo);
        let mut secrets = secrets_with_oanda();
        secrets.oanda_api_key = None;
        let result = acquire_oanda(&meta, &secrets);
        assert!(matches!(result, Err(BrokerError::MissingOandaApiKey)));
    }

    #[test]
    fn oanda_rejects_a_tradenation_account() {
        let mut meta = oanda_meta(Some("x"), AccountKind::Demo);
        meta.broker = BrokerKind::TradeNation;
        let result = acquire_oanda(&meta, &secrets_with_oanda());
        assert!(matches!(result, Err(BrokerError::BrokerMismatch { .. })));
    }

    #[tokio::test]
    async fn tn_rejects_an_oanda_account() {
        // A broker-mismatch is decided before any network login is attempted,
        // so this is safe to assert without the enc store / a live session.
        let meta = oanda_meta(Some("x"), AccountKind::Demo);
        let result = acquire_tn(&meta).await;
        assert!(matches!(result, Err(BrokerError::BrokerMismatch { .. })));
    }
}
