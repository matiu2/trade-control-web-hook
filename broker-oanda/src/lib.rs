//! OANDA implementation of the web hook's broker surface.
//!
//! Wraps `oanda-client` behind a small `OandaBroker` value that holds the
//! authenticated client and the account id. The native runtime constructs it
//! directly via [`OandaBroker::from_api_key`], supplying practice-vs-live
//! explicitly (derived from the account's `kind`) so each account hits its own
//! environment.

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
    list_pending_orders as list_pending_orders_impl,
    lookup_attempt_state as lookup_attempt_state_impl, place_entry,
};
use oanda_client::OandaClient;
use trade_control_core::broker::{
    AmendError, AttemptState, BidAskCandle, Broker, CancelError, Candle, CandleError, EntryError,
    EntryRequest, Granularity, LookupError, OpenPosition, PendingOrder, Quote,
};

/// Authenticated OANDA broker handle. Holds the API client and the account id
/// resolved at construction time.
pub struct OandaBroker {
    client: OandaClient,
    account_id: String,
}

impl OandaBroker {
    /// Construct a broker directly from an API key + sub-account id. This is the
    /// native-runtime entry point (the VM + Postgres binary): it reads
    /// `OANDA_API_KEY` from the process environment / config and the account id
    /// from the Postgres account index. `live` picks the OANDA host (live vs
    /// practice), normally derived from the account's `kind`.
    pub fn from_api_key(api_key: String, account_id: String, live: bool) -> Self {
        let client = if live {
            OandaClient::new_live(api_key)
        } else {
            OandaClient::new(api_key)
        };
        Self { client, account_id }
    }
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

    async fn get_bidask_candles(
        &self,
        instrument: &str,
        granularity: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<BidAskCandle>, CandleError> {
        candles::get_bidask_candles(&self.client, instrument, granularity, since, now).await
    }
}
