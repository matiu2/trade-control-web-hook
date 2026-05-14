//! OANDA implementation of the web hook's broker surface.
//!
//! Wraps `oanda-client` behind a small `OandaBroker` value that holds the
//! authenticated client and the account id. The free `login(env)` helper
//! reads `OANDA_API_KEY`, `OANDA_ACCOUNT_ID`, and the optional `OANDA_LIVE`
//! secret from the Worker `Env`.

mod oanda;
mod risk;

pub use oanda::{EntryError, EntryRequest, OANDA_ACCOUNT_ID};

use oanda::{cancel_pending_for_instrument, close_positions, login as login_client, place_entry};
use oanda_client::OandaClient;
use worker::{Env, console_error};

/// Authenticated OANDA broker handle. Holds the API client and the account id
/// resolved from the Worker secrets at login time.
pub struct OandaBroker {
    client: OandaClient,
    account_id: String,
}

impl OandaBroker {
    /// Risk-gate + place an entry order. Returns the OANDA order id on success.
    pub async fn place_entry(
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

    /// Close all positions for `instrument`. Returns true if anything closed.
    pub async fn close_positions(&self, instrument: &str) -> bool {
        close_positions(&self.client, &self.account_id, instrument).await
    }

    /// Cancel pending orders on `instrument`. Returns the number cancelled.
    pub async fn cancel_pending_for_instrument(&self, instrument: &str) -> usize {
        cancel_pending_for_instrument(&self.client, &self.account_id, instrument).await
    }
}

/// Read the OANDA secrets from `env` and construct a broker. Returns `None`
/// (with errors logged) when a required secret is missing or login fails.
pub async fn login(env: &Env) -> Option<OandaBroker> {
    let account_id = match get_secret(OANDA_ACCOUNT_ID, env) {
        Some(s) => s,
        None => {
            console_error!("missing required secret: {OANDA_ACCOUNT_ID}");
            return None;
        }
    };
    let client = login_client(env).await?;
    Some(OandaBroker { client, account_id })
}

/// Read a secret. Returns `None` if the binding is absent or unreadable.
fn get_secret(name: &str, env: &Env) -> Option<String> {
    env.secret(name).map(|value| value.to_string()).ok()
}
