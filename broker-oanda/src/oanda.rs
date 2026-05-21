//! Layer to interact with oanda.

use oanda_client::OandaClient;
use oanda_client::orders::{
    LimitOrder, OrderPositionFill, OrderType, StopLossDetails, StopOrder, TakeProfitDetails,
    TimeInForce,
};
use oanda_client::positions::ClosePositionResponse;
use worker::Env;
use worker::console_error;

use crate::fx::{quote_currency, resolve_quote_to_account_rate};
use crate::risk;
use trade_control_core::broker::{EntryError, EntryRequest};
use trade_control_core::intent::{Direction, ResolvedEntry, RiskBudget};
use worker::console_log;

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

/// Risk-gate + place the entry order. Returns the OANDA order id on success.
pub async fn place_entry(
    client: &OandaClient,
    account_id: &str,
    max_risk_pct: f64,
    max_open_positions: u32,
    req: &EntryRequest<'_>,
) -> Result<String, EntryError> {
    // Cheap-to-check ceiling for `Percent` mode. `Amount` is checked
    // against the equity-derived percent below once we have equity.
    if let RiskBudget::Percent(pct) = req.risk
        && pct > max_risk_pct
    {
        return Err(EntryError::RiskCapExceeded {
            requested: pct,
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
    let account_ccy = account.account.currency.clone();

    let reference_price = match req.entry {
        ResolvedEntry::Market { reference_price } => reference_price,
        ResolvedEntry::Stop { trigger_price } => trigger_price,
        ResolvedEntry::Limit { trigger_price } => trigger_price,
    };

    // Resolve units + effective percent. For `Units` we skip sizing
    // math entirely and reconstruct the implied money risk for the cap
    // check. For `Amount` we translate to percent so the cap applies.
    // For `Percent` we already validated above; recompute the budget.
    let stop_distance = (reference_price - req.stop_loss).abs();
    if stop_distance <= 0.0 || !stop_distance.is_finite() {
        return Err(EntryError::OrderRejected);
    }

    // Resolve the quote→account FX rate so cross-currency sizing comes
    // out right. Same-ccy short-circuits to 1.0 inside the helper. A
    // failure to resolve rejects the entry — silently falling back to
    // 1.0 would mis-size the trade.
    let quote_ccy = quote_currency(req.instrument);
    let fx_rate = match resolve_quote_to_account_rate(client, account_id, quote_ccy, &account_ccy)
        .await
    {
        Ok(r) => r,
        Err(err) => {
            console_error!(
                "oanda fx resolve failed instrument={} quote={quote_ccy} account_ccy={account_ccy}: {err}",
                req.instrument
            );
            return Err(EntryError::OrderRejected);
        }
    };

    let (units, effective_pct) = match req.risk {
        RiskBudget::Percent(pct) => {
            let budget = equity * pct / 100.0;
            (
                risk::units_for_budget(budget, reference_price, req.stop_loss, fx_rate),
                pct,
            )
        }
        RiskBudget::Amount(amount) => {
            if equity <= 0.0 {
                return Err(EntryError::EquityParse);
            }
            let pct = amount / equity * 100.0;
            if pct > max_risk_pct {
                return Err(EntryError::RiskCapExceeded {
                    requested: pct,
                    cap: max_risk_pct,
                });
            }
            (
                risk::units_for_budget(amount, reference_price, req.stop_loss, fx_rate),
                pct,
            )
        }
        RiskBudget::Units(literal) => {
            if equity <= 0.0 {
                return Err(EntryError::EquityParse);
            }
            // Implied money risk in account ccy = units * stop_distance
            // * fx_rate (quote → account). Divide by equity for percent.
            let implied_amount = literal * stop_distance * fx_rate;
            let pct = implied_amount / equity * 100.0;
            if pct > max_risk_pct {
                return Err(EntryError::RiskCapExceeded {
                    requested: pct,
                    cap: max_risk_pct,
                });
            }
            // OANDA only takes integer units; floor the literal.
            let u = if !literal.is_finite() || literal <= 0.0 {
                0
            } else {
                literal.floor() as u32
            };
            (u, pct)
        }
    };
    let dry = if req.dry_run { "DRY-RUN " } else { "" };
    console_log!(
        "{dry}oanda sizing: instrument={} mode={:?} equity={equity} account_ccy={account_ccy} quote_ccy={quote_ccy} fx_quote_to_account={fx_rate} effective_pct={effective_pct:.4} entry_ref={reference_price} sl={} units={units}",
        req.instrument,
        req.risk,
        req.stop_loss,
    );
    if units == 0 {
        return Err(EntryError::UnitsBelowMinimum);
    }
    if req.dry_run {
        // Sizing succeeded — synthetic order id so the caller treats it
        // as success. The order is intentionally not placed.
        return Ok(format!("dry-run-{}", req.instrument));
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
