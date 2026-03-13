//! Layer to interact with oanda

use oanda_client::OandaClient;
use oanda_client::positions::ClosePositionResponse;
use worker::Env;
use worker::console_error;

const OANDA_API_KEY: &str = "OANDA_API_KEY";
const OANDA_ACCOUNT_ID: &str = "OANDA_ACCOUNT_ID";
const OANDA_LIVE: &str = "OANDA_LIVE";

/// Log in to oanda - if it can't it'll log the error and give you nothing
pub async fn login(env: &Env) -> Option<OandaClient> {
    let api_key = crate::get_secret(OANDA_API_KEY, env)?;
    let live = crate::get_secret(OANDA_LIVE, env)
        .and_then(|s| (s.to_lowercase() == "true").then_some(true))
        .unwrap_or(false);
    Some(if live {
        OandaClient::new_live(api_key)
    } else {
        OandaClient::new(api_key)
    })
}

/// Closes all positions for a selected instrument. Returns true if any positions were closed;
/// false if there were no positions, or there was an error; errors are logged to the console
pub async fn close_positions(client: &OandaClient, env: &Env, instrument_name: &str) -> bool {
    let Some(account_id) = crate::get_secret(OANDA_ACCOUNT_ID, env) else {
        return false;
    };
    match client.close_position(&account_id, instrument_name).await {
        Ok(ClosePositionResponse {
            related_transaction_ids,
            ..
        }) => {
            // If any transaction was created, consider it a success
            !related_transaction_ids.is_empty()
        }
        Err(err) => {
            console_error!("Error closing positions: {err:?}");
            false
        }
    }
}
