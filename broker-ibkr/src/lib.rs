//! Interactive Brokers implementation of the web hook's broker surface.
//!
//! Wraps [`ibkr_client`] behind an [`IbkrBroker`] value holding a connected
//! Gateway client and the account id to trade. Mirrors `broker-oanda`'s shape:
//! a thin `Broker` impl delegating to a private module, with sizing and FX kept
//! private so they are implementation detail rather than trait surface.
//!
//! # What makes this broker different
//!
//! Everything the `Broker` trait exposes maps onto futures unchanged — the
//! trait needed no futures-specific additions, and TradeNation already proved
//! it product-neutral by putting *stake* where OANDA puts *units*. IBKR puts
//! **contracts**. Three quanta, one `Option<f64>`.
//!
//! What genuinely differs sits *below* the trait:
//!
//! - **Sizing is integral and multiplied.** One contract is the quantum, and a
//!   contract multiplier converts price movement into money. See [`risk`].
//! - **The session is a local process, not a token.** IBKR issues no bearer
//!   credential to a retail account; a Java Gateway holds the session and this
//!   crate speaks to its socket. A reconnect is a *local* failure mode with no
//!   OANDA/TradeNation analogue.
//! - **A contract expires**, and IBKR force-liquidates during a close-out
//!   window that for physically-delivered gold begins roughly a *month* before
//!   the printed expiry. That is guarded at **arm time** (Stage 3), not here —
//!   by the time an order reaches this crate it is far too late to refuse on
//!   calendar grounds.
//!
//! # Status
//!
//! The read path (contract chains, multipliers, size limits) is proven against
//! a live paper Gateway. **The order path is not yet exercised** — see
//! `TODO-broker-ibkr.md`.

mod ibkr;
mod risk;

use chrono::{DateTime, Utc};
use trade_control_core::broker::{
    AmendError, AttemptState, BidAskCandle, Broker, CancelError, Candle, CandleError, CloseOutcome,
    EntryError, EntryRequest, Granularity, LookupError, OpenPosition, PendingOrder, Placement,
    Quote,
};

pub use ibkr::{IbkrError, PAPER_GATEWAY};

/// Authenticated IBKR broker handle: a connected Gateway client plus the
/// account it trades.
///
/// Unlike OANDA (a stateless HTTPS client) this holds a **live socket** to a
/// local Gateway process. The Gateway forces a daily restart and a weekly
/// re-authentication, so a long-lived handle must tolerate the connection
/// dropping underneath it; a supervisor (IBC) keeps the Gateway itself up.
pub struct IbkrBroker {
    client: ibapi::prelude::Client,
    account_id: String,
}

impl IbkrBroker {
    /// Connect to a running IB Gateway and bind the account to trade.
    ///
    /// `client_id` must be unique among everything connected to that Gateway —
    /// reusing an id already in use is rejected by the Gateway itself.
    ///
    /// Registers the Australian timezone aliases first: the Gateway announces
    /// its timezone as an abbreviation during the handshake and an
    /// unrecognised one is a **hard connect failure**, not a degrade.
    pub async fn connect(
        address: &str,
        client_id: i32,
        account_id: String,
    ) -> Result<Self, IbkrError> {
        ibkr_client::register_timezone_aliases();
        let client = ibkr_client::connect(address, client_id)
            .await
            .map_err(|err| IbkrError::Connect(err.to_string()))?;
        Ok(Self { client, account_id })
    }

    /// The account this handle trades.
    pub fn account_id(&self) -> &str {
        &self.account_id
    }
}

impl Broker for IbkrBroker {
    async fn place_entry(
        &self,
        max_risk_pct: f64,
        max_open_positions: u32,
        req: &EntryRequest<'_>,
    ) -> Result<Placement, EntryError> {
        ibkr::place_entry(
            &self.client,
            &self.account_id,
            max_risk_pct,
            max_open_positions,
            req,
        )
        .await
    }

    async fn close_positions(&self, instrument: &str) -> CloseOutcome {
        ibkr::close_positions(&self.client, &self.account_id, instrument).await
    }

    async fn cancel_pending_for_instrument(&self, instrument: &str) -> usize {
        ibkr::cancel_pending_for_instrument(&self.client, &self.account_id, instrument).await
    }

    async fn lookup_attempt_state(
        &self,
        instrument: &str,
        broker_order_id: &str,
        broker_trade_id: Option<&str>,
    ) -> Result<AttemptState, LookupError> {
        ibkr::lookup_attempt_state(
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
        // The account is bound at construction, same as OANDA; the trait-level
        // argument is ignored.
        ibkr::cancel_order(&self.client, broker_order_id).await
    }

    async fn get_quote(&self, instrument: &str) -> Result<Quote, LookupError> {
        ibkr::get_quote(&self.client, instrument).await
    }

    async fn list_open_positions(
        &self,
        _account_id: &str,
    ) -> Result<Vec<OpenPosition>, LookupError> {
        ibkr::list_open_positions(&self.client, &self.account_id).await
    }

    async fn amend_stop(
        &self,
        _account_id: &str,
        position_or_order_id: &str,
        new_stop: f64,
    ) -> Result<(), AmendError> {
        ibkr::amend_stop(&self.client, position_or_order_id, new_stop).await
    }

    async fn list_pending_orders(
        &self,
        _account_id: &str,
    ) -> Result<Vec<PendingOrder>, LookupError> {
        ibkr::list_pending_orders(&self.client, &self.account_id).await
    }

    async fn get_candles(
        &self,
        instrument: &str,
        granularity: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Candle>, CandleError> {
        ibkr::get_candles(&self.client, instrument, granularity, since, now).await
    }

    async fn get_bidask_candles(
        &self,
        instrument: &str,
        granularity: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<BidAskCandle>, CandleError> {
        ibkr::get_bidask_candles(&self.client, instrument, granularity, since, now).await
    }
}
