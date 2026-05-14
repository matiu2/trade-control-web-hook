//! Layer to interact with oanda.

use oanda_client::OandaClient;
use oanda_client::orders::{
    LimitOrder, OrderPositionFill, OrderType, StopLossDetails, StopOrder, TakeProfitDetails,
    TimeInForce,
};
use oanda_client::positions::ClosePositionResponse;
use worker::Env;
use worker::console_error;

use crate::risk;
use trade_control_core::intent::{Direction, ResolvedEntry};

const OANDA_API_KEY: &str = "OANDA_API_KEY";
pub const OANDA_ACCOUNT_ID: &str = "OANDA_ACCOUNT_ID";
const OANDA_LIVE: &str = "OANDA_LIVE";

/// Log in to oanda - if it can't it'll log the error and give you nothing
pub async fn login(env: &Env) -> Option<OandaClient> {
    let api_key = match super::get_secret(OANDA_API_KEY, env) {
        Some(s) => s,
        None => {
            console_error!("missing required secret: {OANDA_API_KEY}");
            return None;
        }
    };
    let live = super::get_secret(OANDA_LIVE, env)
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
pub async fn close_positions(
    client: &OandaClient,
    account_id: &str,
    instrument_name: &str,
) -> bool {
    match client.close_position(account_id, instrument_name).await {
        Ok(ClosePositionResponse {
            related_transaction_ids,
            ..
        }) => !related_transaction_ids.is_empty(),
        Err(err) => {
            console_error!("Error closing positions: {err:?}");
            false
        }
    }
}

/// Cancel any pending orders on `instrument` for this account. Used when an
/// `invalidate` action fires — a setup that's been invalidated shouldn't still
/// have a live stop-entry on the book.
pub async fn cancel_pending_for_instrument(
    client: &OandaClient,
    account_id: &str,
    instrument: &str,
) -> usize {
    let pending = match client.get_pending_orders(account_id).await {
        Ok(p) => p,
        Err(err) => {
            console_error!("Error fetching pending orders: {err:?}");
            return 0;
        }
    };
    let mut cancelled = 0;
    for order in pending.into_iter().filter(|o| o.instrument == instrument) {
        match client.cancel_order(account_id, &order.id).await {
            Ok(_) => cancelled += 1,
            Err(err) => console_error!("Error cancelling order {}: {err:?}", order.id),
        }
    }
    cancelled
}

#[derive(Debug)]
pub enum EntryError {
    AccountFetch,
    EquityParse,
    RiskCapExceeded { requested: f64, cap: f64 },
    OpenPositionsCapExceeded,
    UnitsBelowMinimum,
    OrderRejected,
}

impl core::fmt::Display for EntryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AccountFetch => f.write_str("failed to fetch account"),
            Self::EquityParse => f.write_str("failed to parse account equity"),
            Self::RiskCapExceeded { requested, cap } => {
                write!(f, "risk {requested}% > cap {cap}%")
            }
            Self::OpenPositionsCapExceeded => f.write_str("open positions cap exceeded"),
            Self::UnitsBelowMinimum => f.write_str("computed units below minimum"),
            Self::OrderRejected => f.write_str("OANDA rejected the order"),
        }
    }
}

impl std::error::Error for EntryError {}

/// Resolved input for `place_entry`. Holds only what the OANDA call needs.
pub struct EntryRequest<'a> {
    pub instrument: &'a str,
    pub direction: Direction,
    pub entry: ResolvedEntry,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub risk_pct: f64,
}

/// Risk-gate + place the entry order. Returns the OANDA order id on success.
pub async fn place_entry(
    client: &OandaClient,
    account_id: &str,
    max_risk_pct: f64,
    max_open_positions: u32,
    req: &EntryRequest<'_>,
) -> Result<String, EntryError> {
    if req.risk_pct > max_risk_pct {
        return Err(EntryError::RiskCapExceeded {
            requested: req.risk_pct,
            cap: max_risk_pct,
        });
    }

    let account = client.get_account(account_id).await.map_err(|err| {
        console_error!("get_account: {err:?}");
        EntryError::AccountFetch
    })?;

    if (account.account.open_position_count as u32) >= max_open_positions {
        return Err(EntryError::OpenPositionsCapExceeded);
    }

    let equity: f64 = account
        .account
        .nav
        .parse()
        .map_err(|_| EntryError::EquityParse)?;

    let reference_price = match req.entry {
        ResolvedEntry::Market { reference_price } => reference_price,
        ResolvedEntry::Stop { trigger_price } => trigger_price,
        ResolvedEntry::Limit { trigger_price } => trigger_price,
    };

    let units = risk::units_for_risk(equity, req.risk_pct, reference_price, req.stop_loss);
    if units == 0 {
        return Err(EntryError::UnitsBelowMinimum);
    }

    let sl_details = StopLossDetails {
        price: Some(format_price(req.stop_loss)),
        distance: None,
        time_in_force: TimeInForce::Gtc,
    };
    let tp_details = TakeProfitDetails {
        price: format_price(req.take_profit),
        time_in_force: TimeInForce::Gtc,
    };

    let response = match (&req.entry, req.direction) {
        (ResolvedEntry::Market { .. }, Direction::Long) => client
            .buy_with_stops(
                account_id,
                req.instrument,
                units,
                Some(sl_details),
                Some(tp_details),
                None,
            )
            .await
            .map_err(|err| {
                console_error!("buy_with_stops: {err:?}");
                EntryError::OrderRejected
            })?,
        (ResolvedEntry::Market { .. }, Direction::Short) => client
            .sell_with_stops(
                account_id,
                req.instrument,
                units,
                Some(sl_details),
                Some(tp_details),
                None,
            )
            .await
            .map_err(|err| {
                console_error!("sell_with_stops: {err:?}");
                EntryError::OrderRejected
            })?,
        (ResolvedEntry::Stop { trigger_price }, dir) => {
            let signed_units = match dir {
                Direction::Long => units.to_string(),
                Direction::Short => format!("-{units}"),
            };
            let order = StopOrder {
                r#type: OrderType::Stop,
                instrument: req.instrument.to_string(),
                units: signed_units,
                price: format_price(*trigger_price),
                time_in_force: TimeInForce::Gtc,
                position_fill: OrderPositionFill::Default,
                take_profit_on_fill: Some(tp_details),
                stop_loss_on_fill: Some(sl_details),
                trailing_stop_loss_on_fill: None,
            };
            client
                .place_stop_order(account_id, order)
                .await
                .map_err(|err| {
                    console_error!("place_stop_order: {err:?}");
                    EntryError::OrderRejected
                })?
        }
        (ResolvedEntry::Limit { trigger_price }, dir) => {
            let signed_units = match dir {
                Direction::Long => units.to_string(),
                Direction::Short => format!("-{units}"),
            };
            let order = LimitOrder {
                r#type: OrderType::Limit,
                instrument: req.instrument.to_string(),
                units: signed_units,
                price: format_price(*trigger_price),
                time_in_force: TimeInForce::Gtc,
                position_fill: OrderPositionFill::Default,
                take_profit_on_fill: Some(tp_details),
                stop_loss_on_fill: Some(sl_details),
                trailing_stop_loss_on_fill: None,
            };
            client
                .place_limit_order(account_id, order)
                .await
                .map_err(|err| {
                    console_error!("place_limit_order: {err:?}");
                    EntryError::OrderRejected
                })?
        }
    };

    Ok(response.order_create_transaction.id)
}

/// OANDA prices are strings with instrument-appropriate precision. Five decimal
/// places covers all major FX pairs except JPY crosses (3); indices use 1-2.
/// OANDA will round to the instrument's tick on its end; over-precision is fine.
fn format_price(price: f64) -> String {
    format!("{price:.5}")
}
