//! OANDA implementation of the web hook's broker surface.
//!
//! Wraps `oanda-client` behind a small `OandaBroker` value that holds the
//! authenticated client and the account id. The free `login(env)` helper
//! reads `OANDA_API_KEY`, `OANDA_ACCOUNT_ID`, and the optional `OANDA_LIVE`
//! secret from the Worker `Env`.

mod fx;
mod oanda;
mod risk;

pub use oanda::OANDA_ACCOUNT_ID;

use oanda::{cancel_pending_for_instrument, close_positions, login as login_client, place_entry};
use oanda_client::OandaClient;
use trade_control_core::broker::{AttemptState, Broker, EntryError, EntryRequest, LookupError};
use worker::{Env, console_error};

/// Authenticated OANDA broker handle. Holds the API client and the account id
/// resolved from the Worker secrets at login time.
pub struct OandaBroker {
    client: OandaClient,
    account_id: String,
}

impl Broker for OandaBroker {
    async fn place_entry(
        &self,
        max_risk_pct: f64,
        max_open_positions: u32,
        req: &EntryRequest<'_>,
    ) -> Result<String, EntryError> {
        place_entry(
            &self.client,
            &self.account_id,
            max_risk_pct,
            max_open_positions,
            req,
        )
        .await
    }

    async fn close_positions(&self, instrument: &str) -> bool {
        close_positions(&self.client, &self.account_id, instrument).await
    }

    async fn cancel_pending_for_instrument(&self, instrument: &str) -> usize {
        cancel_pending_for_instrument(&self.client, &self.account_id, instrument).await
    }

    // TODO(1b): real implementation lives in the next sub-step. Returning
    // `Transient` makes the retry gate reject the fire (no order placed)
    // and surfaces the missing impl in logs immediately, rather than
    // silently advancing the attempt counter on a hard-coded answer.
    async fn lookup_attempt_state(
        &self,
        _instrument: &str,
        _broker_order_id: &str,
        _broker_trade_id: Option<&str>,
    ) -> Result<AttemptState, LookupError> {
        Err(LookupError::Transient)
    }
}

/// Read the OANDA secrets from `env` and construct a broker. Returns `None`
/// (with errors logged) when a required secret is missing or login fails.
///
/// Uses the worker-global `OANDA_ACCOUNT_ID` secret for the sub-account
/// id — call [`login_with_account_id`] instead when the intent names a
/// specific account whose id lives on its metadata record.
pub async fn login(env: &Env) -> Option<OandaBroker> {
    let account_id = match get_secret(OANDA_ACCOUNT_ID, env) {
        Some(s) => s,
        None => {
            console_error!("missing required secret: {OANDA_ACCOUNT_ID}");
            return None;
        }
    };
    login_with_account_id(env, account_id).await
}

/// Like [`login`] but uses an explicitly-supplied `account_id` (e.g.
/// from per-account metadata) instead of reading `OANDA_ACCOUNT_ID`.
/// The API token is still the shared worker-wide `OANDA_API_KEY`.
pub async fn login_with_account_id(env: &Env, account_id: String) -> Option<OandaBroker> {
    let client = login_client(env).await?;
    Some(OandaBroker { client, account_id })
}

/// Read a secret. Returns `None` if the binding is absent or unreadable.
fn get_secret(name: &str, env: &Env) -> Option<String> {
    env.secret(name).map(|value| value.to_string()).ok()
}
