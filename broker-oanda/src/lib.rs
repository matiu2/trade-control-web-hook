//! OANDA implementation of the web hook's broker surface.
//!
//! Wraps `oanda-client` behind a small `OandaBroker` value that holds the
//! authenticated client and the account id. The free `login(env)` helper
//! reads `OANDA_API_KEY`, `OANDA_ACCOUNT_ID`, and the optional `OANDA_LIVE`
//! secret from the Worker `Env`.

mod candles;
mod fx;
mod oanda;
mod risk;

pub use oanda::OANDA_ACCOUNT_ID;

use chrono::{DateTime, Utc};
use oanda::{
    amend_stop as amend_stop_impl, cancel_order as cancel_order_impl,
    cancel_pending_for_instrument, close_positions, get_quote as get_quote_impl,
    list_open_positions as list_open_positions_impl,
    list_pending_orders as list_pending_orders_impl, login as login_client,
    lookup_attempt_state as lookup_attempt_state_impl, place_entry,
};
use oanda_client::OandaClient;
use trade_control_core::broker::{
    AmendError, AttemptState, Broker, CancelError, Candle, CandleError, EntryError, EntryRequest,
    Granularity, LookupError, OpenPosition, PendingOrder, Quote,
};
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

    async fn lookup_attempt_state(
        &self,
        instrument: &str,
        broker_order_id: &str,
        broker_trade_id: Option<&str>,
    ) -> Result<AttemptState, LookupError> {
        lookup_attempt_state_impl(
            &self.client,
            &self.account_id,
            instrument,
            broker_order_id,
            broker_trade_id,
        )
        .await
    }

    async fn cancel_order(
        &self,
        _account_id: &str,
        broker_order_id: &str,
    ) -> Result<(), CancelError> {
        // OANDA's account id is bound at `OandaBroker` construction
        // time (resolved from secrets or per-account metadata), so
        // we ignore the trait-level argument and use the stored one.
        // The trait still takes it because TradeNation may want to
        // pass per-call account context one day.
        cancel_order_impl(&self.client, &self.account_id, broker_order_id).await
    }

    async fn get_quote(&self, instrument: &str) -> Result<Quote, LookupError> {
        get_quote_impl(&self.client, &self.account_id, instrument).await
    }

    async fn list_open_positions(
        &self,
        _account_id: &str,
    ) -> Result<Vec<OpenPosition>, LookupError> {
        // OANDA's account id is bound at construction; the trait-level
        // argument is ignored (same as `cancel_order`).
        list_open_positions_impl(&self.client, &self.account_id).await
    }

    async fn amend_stop(
        &self,
        _account_id: &str,
        position_or_order_id: &str,
        new_stop: f64,
    ) -> Result<(), AmendError> {
        amend_stop_impl(
            &self.client,
            &self.account_id,
            position_or_order_id,
            new_stop,
        )
        .await
    }

    async fn list_pending_orders(
        &self,
        _account_id: &str,
    ) -> Result<Vec<PendingOrder>, LookupError> {
        list_pending_orders_impl(&self.client, &self.account_id).await
    }

    async fn get_candles(
        &self,
        instrument: &str,
        granularity: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Candle>, CandleError> {
        candles::get_candles(&self.client, instrument, granularity, since, now).await
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
