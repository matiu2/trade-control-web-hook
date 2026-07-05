//! Layer to interact with oanda.

use oanda_client::OandaClient;
use oanda_client::orders::{
    LimitOrder, OrderPositionFill, OrderType, PendingOrder, StopLossDetails, StopOrder,
    TakeProfitDetails, TimeInForce,
};
use oanda_client::positions::ClosePositionResponse;
use oanda_client::trades::{Trade, TradeQueryParams, TradeState};
#[cfg(target_arch = "wasm32")]
use worker::Env;

use crate::fx::{quote_currency, resolve_quote_to_account_rate};
use crate::risk;
use trade_control_core::broker::{
    AmendError, AttemptState, CancelError, EntryError, EntryRequest, LookupError, OpenPosition,
    PendingOrder as CorePendingOrder, Quote,
};
use trade_control_core::intent::{Direction, ResolvedEntry, RiskBudget};

/// Closed-trade scan window. Plan §3 recommends ~50 — large enough to
/// catch any reasonable retry-window trade, small enough to keep the
/// per-fire round-trip cheap.
const CLOSED_TRADE_SCAN_COUNT: i32 = 50;

#[cfg(target_arch = "wasm32")]
const OANDA_API_KEY: &str = "OANDA_API_KEY";
pub const OANDA_ACCOUNT_ID: &str = "OANDA_ACCOUNT_ID";
#[cfg(target_arch = "wasm32")]
const OANDA_LIVE: &str = "OANDA_LIVE";

/// Parse the worker-global `OANDA_LIVE` secret into a live/practice flag.
/// Absent / non-`true` → practice. Pure so the parsing is unit-testable
/// without a Worker `Env`.
#[cfg(any(target_arch = "wasm32", test))]
pub(super) fn live_flag_from_secret(raw: Option<String>) -> bool {
    raw.map(|s| s.to_lowercase() == "true").unwrap_or(false)
}

/// Log in to oanda using the worker-global `OANDA_LIVE` secret to pick
/// practice vs live. Used by the legacy global path (`account: None`).
/// If it can't, it'll log the error and give you nothing.
#[cfg(target_arch = "wasm32")]
pub async fn login(env: &Env) -> Option<OandaClient> {
    let live = live_flag_from_secret(super::get_secret(OANDA_LIVE, env));
    login_with_live(env, live).await
}

/// Like [`login`] but the caller supplies the live/practice flag
/// directly — e.g. derived from a named account's `kind` so each account
/// hits its own OANDA environment regardless of the global `OANDA_LIVE`.
#[cfg(target_arch = "wasm32")]
pub async fn login_with_live(env: &Env, live: bool) -> Option<OandaClient> {
    let api_key = match super::get_secret(OANDA_API_KEY, env) {
        Some(s) => s,
        None => {
            tracing::error!("missing required secret: {OANDA_API_KEY}");
            return None;
        }
    };
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
            tracing::error!("Error closing positions: {err:?}");
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
            tracing::error!("Error fetching pending orders: {err:?}");
            return 0;
        }
    };
    let mut cancelled = 0;
    for order in pending.into_iter().filter(|o| o.instrument == instrument) {
        match client.cancel_order(account_id, &order.id).await {
            Ok(_) => cancelled += 1,
            Err(err) => tracing::error!("Error cancelling order {}: {err:?}", order.id),
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
        tracing::error!("get_account: {err:?}");
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
            tracing::error!(
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
    tracing::info!(
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
                tracing::error!("buy_with_stops: {err:?}");
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
                tracing::error!("sell_with_stops: {err:?}");
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
                    tracing::error!("place_stop_order: {err:?}");
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
                    tracing::error!("place_limit_order: {err:?}");
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

/// Look up the broker-side state of an entry attempt placed earlier.
/// Implements plan §3's four-step algorithm against the OANDA REST API.
/// Any network / 5xx failure surfaces as `LookupError::Transient` so the
/// caller rejects the current fire without placing.
pub async fn lookup_attempt_state(
    client: &OandaClient,
    account_id: &str,
    instrument: &str,
    broker_order_id: &str,
    broker_trade_id: Option<&str>,
) -> Result<AttemptState, LookupError> {
    let pending = client.get_pending_orders(account_id).await.map_err(|err| {
        tracing::error!("oanda lookup get_pending_orders: {err:?}");
        LookupError::Transient
    })?;

    let open_params = TradeQueryParams::new()
        .state(TradeState::Open)
        .instrument(instrument);
    let open = client
        .get_trades(account_id, Some(open_params))
        .await
        .map_err(|err| {
            tracing::error!("oanda lookup get_trades(Open): {err:?}");
            LookupError::Transient
        })?;

    // Only fetch closed trades if we have a broker_trade_id to match —
    // step 3 only runs in that case, and the closed-trades request is
    // the most expensive of the three.
    let closed = if broker_trade_id.is_some() {
        let closed_params = TradeQueryParams::new()
            .state(TradeState::Closed)
            .instrument(instrument)
            .count(CLOSED_TRADE_SCAN_COUNT);
        let trades = client
            .get_trades(account_id, Some(closed_params))
            .await
            .map_err(|err| {
                tracing::error!("oanda lookup get_trades(Closed): {err:?}");
                LookupError::Transient
            })?;
        Some(trades)
    } else {
        None
    };

    Ok(compute_attempt_state(
        broker_order_id,
        broker_trade_id,
        &pending,
        &open,
        closed.as_deref(),
    ))
}

/// Cancel a specific pending order by id. Wraps the OANDA REST call
/// and maps any non-success into `CancelError::Transient` (per the
/// plan's race-handling note — the caller treats this as "probably
/// filled, re-lookup" rather than retrying).
pub async fn cancel_order(
    client: &OandaClient,
    account_id: &str,
    broker_order_id: &str,
) -> Result<(), CancelError> {
    client
        .cancel_order(account_id, broker_order_id)
        .await
        .map(|_| ())
        .map_err(|err| {
            tracing::error!("oanda cancel_order({broker_order_id}): {err:?}");
            CancelError::Transient
        })
}

/// Fetch a live [`Quote`] for `instrument` via OANDA's pricing endpoint.
/// The trait's default `get_current_price` takes the mid (exact parity with
/// the old `tick.mid()`); the spread-blackout systems read `spread()`.
pub async fn get_quote(
    client: &OandaClient,
    account_id: &str,
    instrument: &str,
) -> Result<Quote, LookupError> {
    let pricing = client
        .get_pricing(account_id, &[instrument])
        .await
        .map_err(|err| {
            tracing::error!("oanda get_pricing({instrument}): {err:?}");
            LookupError::Transient
        })?;
    let tick = pricing.prices.first().ok_or_else(|| {
        tracing::error!("oanda get_pricing({instrument}): empty prices array");
        LookupError::Transient
    })?;
    let bid = tick.best_bid().ok_or_else(|| {
        tracing::error!("oanda get_pricing({instrument}): missing bid");
        LookupError::Transient
    })?;
    let ask = tick.best_ask().ok_or_else(|| {
        tracing::error!("oanda get_pricing({instrument}): missing ask");
        LookupError::Transient
    })?;
    Ok(Quote { bid, ask })
}

/// All open positions on the account. OANDA reports open trades; we map
/// each to an [`OpenPosition`]. Direction comes from the sign of
/// `current_units` (positive = long), stop / take-profit from the trade's
/// dependent orders. OANDA has no separate originating-order id once a
/// trade is open, so `order_id` and `position_id` both carry the trade id.
pub async fn list_open_positions(
    client: &OandaClient,
    account_id: &str,
) -> Result<Vec<OpenPosition>, LookupError> {
    let params = TradeQueryParams {
        state: Some(TradeState::Open),
        ..Default::default()
    };
    let trades = client
        .get_trades(account_id, Some(params))
        .await
        .map_err(|err| {
            tracing::error!("oanda list_open_positions get_trades: {err:?}");
            LookupError::Transient
        })?;
    Ok(trades.iter().map(oanda_trade_to_open).collect())
}

/// All resting (unfilled) entry orders on the account.
pub async fn list_pending_orders(
    client: &OandaClient,
    account_id: &str,
) -> Result<Vec<CorePendingOrder>, LookupError> {
    let orders = client.get_pending_orders(account_id).await.map_err(|err| {
        tracing::error!("oanda list_pending_orders get_pending_orders: {err:?}");
        LookupError::Transient
    })?;
    Ok(orders.iter().filter_map(oanda_order_to_pending).collect())
}

/// Move the stop-loss on an open trade to `new_stop`, leaving the take-profit
/// untouched. `position_or_order_id` is the trade id. Unmatched → `NotFound`.
pub async fn amend_stop(
    client: &OandaClient,
    account_id: &str,
    position_or_order_id: &str,
    new_stop: f64,
) -> Result<(), AmendError> {
    // Confirm the trade exists so an unknown id is `NotFound`, not a
    // silently-successful no-op (OANDA would 404 the modify otherwise).
    let trade = client
        .get_trade(account_id, position_or_order_id)
        .await
        .map_err(|err| {
            tracing::error!("oanda amend_stop get_trade({position_or_order_id}): {err:?}");
            // OANDA returns an error (404) for an unknown trade id; we
            // can't cheaply distinguish 404 from a transient 5xx here, so
            // treat "couldn't fetch the trade" as NotFound — the caller
            // re-lists on the next sweep regardless.
            AmendError::NotFound
        })?;
    let stop_loss = StopLossDetails {
        price: Some(format_price(new_stop)),
        distance: None,
        time_in_force: TimeInForce::Gtc,
    };
    client
        .modify_trade_stops(account_id, &trade.id, Some(stop_loss), None, None)
        .await
        .map(|_| ())
        .map_err(|err| {
            tracing::error!("oanda amend_stop modify_trade_stops({}): {err:?}", trade.id);
            AmendError::Transient
        })
}

/// Map an OANDA open [`Trade`] to a broker-agnostic [`OpenPosition`].
/// Direction from the sign of `current_units`; SL/TP from dependent orders.
fn oanda_trade_to_open(t: &Trade) -> OpenPosition {
    let units: f64 = t.current_units.parse().unwrap_or(0.0);
    OpenPosition {
        instrument: t.instrument.clone(),
        direction: if units < 0.0 {
            Direction::Short
        } else {
            Direction::Long
        },
        stop_loss: t.stop_loss_order.as_ref().and_then(|o| o.price),
        take_profit: t.take_profit_order.as_ref().and_then(|o| o.price),
        position_id: t.id.clone(),
        order_id: t.id.clone(),
        stake: units.abs(),
    }
}

/// Map an OANDA resting [`PendingOrder`] to a broker-agnostic
/// [`CorePendingOrder`]. Returns `None` for non-entry order types (e.g.
/// stray STOP_LOSS / TAKE_PROFIT orders) or an unparseable trigger price.
fn oanda_order_to_pending(o: &PendingOrder) -> Option<CorePendingOrder> {
    let is_stop = match o.r#type {
        OrderType::Stop => true,
        OrderType::Limit => false,
        // Not an entry order — skip (SL/TP/trailing orders surface here too).
        _ => return None,
    };
    let trigger: f64 = o.price.parse().ok()?;
    let units: f64 = o.units.parse().unwrap_or(0.0);
    Some(CorePendingOrder {
        order_id: o.id.clone(),
        instrument: o.instrument.clone(),
        direction: if units < 0.0 {
            Direction::Short
        } else {
            Direction::Long
        },
        trigger,
        is_stop,
        stake: units.abs(),
    })
}

/// Pure helper running the four-step algorithm against pre-fetched
/// data. Split out from [`lookup_attempt_state`] so unit tests can
/// exercise every branch without needing a live OANDA client.
///
/// `closed` is `None` when no `broker_trade_id` is available — the
/// caller skips the fetch entirely in that case (cheaper, and step
/// 3 cannot match anyway).
fn compute_attempt_state(
    broker_order_id: &str,
    broker_trade_id: Option<&str>,
    pending: &[PendingOrder],
    open: &[Trade],
    closed: Option<&[Trade]>,
) -> AttemptState {
    // 1. Pending → resting unfilled.
    if pending.iter().any(|o| o.id == broker_order_id) {
        return AttemptState::Pending;
    }

    // 2. Open trade whose originating order id matches → still open.
    //    OANDA reuses the create-order transaction id as the trade id,
    //    so trade.id == broker_order_id here. Matching on trade.id
    //    keeps the structure parallel to the TN side (which has to
    //    match an explicit originating field).
    if let Some(trade) = open.iter().find(|t| t.id == broker_order_id) {
        return AttemptState::OpenPosition {
            broker_trade_id: trade.id.clone(),
        };
    }

    // 3. Closed trade lookup — only if we previously snapshotted a
    //    broker_trade_id. Bucket realised P&L: > 0 wins, otherwise
    //    loss/breakeven.
    if let (Some(btid), Some(closed_trades)) = (broker_trade_id, closed)
        && let Some(trade) = closed_trades.iter().find(|t| t.id == btid)
    {
        return if trade.realized_pl > 0.0 {
            AttemptState::ClosedWin {
                realized_pl: trade.realized_pl,
            }
        } else {
            AttemptState::ClosedLossOrBreakeven {
                realized_pl: trade.realized_pl,
            }
        };
    }

    // 4. Nowhere to be found. Distinguish so logs can tell us
    //    whether we lost a snapshot or never had one.
    if broker_trade_id.is_some() {
        AttemptState::Cancelled
    } else {
        AttemptState::Unknown
    }
}

#[cfg(test)]
mod attempt_state_tests {
    use super::*;
    use oanda_client::trades::{Trade, TradeState};

    fn make_pending(id: &str) -> PendingOrder {
        PendingOrder {
            id: id.into(),
            r#type: OrderType::Stop,
            instrument: "EUR_USD".into(),
            units: "100".into(),
            price: "1.10000".into(),
            time_in_force: TimeInForce::Gtc,
            create_time: String::new(),
            take_profit_on_fill: None,
            stop_loss_on_fill: None,
        }
    }

    fn make_trade(id: &str, state: TradeState, realized_pl: f64) -> Trade {
        Trade {
            id: id.into(),
            instrument: "EUR_USD".into(),
            current_units: "100".into(),
            price: 1.10000,
            open_time: String::new(),
            state,
            initial_units: "100".into(),
            initial_margin_required: 0.0,
            margin_used: None,
            unrealized_pl: None,
            realized_pl,
            average_close_price: None,
            close_time: None,
            closing_transaction_ids: None,
            financing: 0.0,
            dividend_adjustment: 0.0,
            take_profit_order: None,
            stop_loss_order: None,
            trailing_stop_loss_order: None,
        }
    }

    #[test]
    fn pending_when_order_id_in_pending_list() {
        let pending = vec![make_pending("ord-1"), make_pending("ord-2")];
        let s = compute_attempt_state("ord-1", None, &pending, &[], None);
        assert_eq!(s, AttemptState::Pending);
    }

    #[test]
    fn open_position_returns_trade_id_as_broker_trade_id() {
        // OANDA: trade.id == originating order id, so the returned
        // broker_trade_id equals the lookup's order id.
        let open = vec![make_trade("ord-7", TradeState::Open, 0.0)];
        let s = compute_attempt_state("ord-7", None, &[], &open, None);
        assert_eq!(
            s,
            AttemptState::OpenPosition {
                broker_trade_id: "ord-7".into()
            }
        );
    }

    #[test]
    fn closed_win_when_positive_realized_pl() {
        let closed = vec![make_trade("ord-3", TradeState::Closed, 12.5)];
        let s = compute_attempt_state("ord-3", Some("ord-3"), &[], &[], Some(&closed));
        assert_eq!(s, AttemptState::ClosedWin { realized_pl: 12.5 });
    }

    #[test]
    fn closed_loss_or_breakeven_when_non_positive_pl() {
        let closed = vec![
            make_trade("ord-4", TradeState::Closed, -7.0),
            make_trade("ord-5", TradeState::Closed, 0.0),
        ];
        let loss = compute_attempt_state("ord-4", Some("ord-4"), &[], &[], Some(&closed));
        assert_eq!(
            loss,
            AttemptState::ClosedLossOrBreakeven { realized_pl: -7.0 }
        );
        let breakeven = compute_attempt_state("ord-5", Some("ord-5"), &[], &[], Some(&closed));
        assert_eq!(
            breakeven,
            AttemptState::ClosedLossOrBreakeven { realized_pl: 0.0 }
        );
    }

    #[test]
    fn cancelled_when_snapshotted_but_not_in_closed_window() {
        // We had a broker_trade_id (so it was open at some point) but
        // it isn't pending, isn't open, and isn't in the closed-trade
        // scan window. Treat as cancelled.
        let s = compute_attempt_state("ord-9", Some("ord-9"), &[], &[], Some(&[]));
        assert_eq!(s, AttemptState::Cancelled);
    }

    #[test]
    fn unknown_when_no_snapshot_and_not_pending_or_open() {
        // No broker_trade_id snapshot and nothing to be found anywhere.
        // We lost track; distinct from Cancelled because the cause is
        // "we never snapshotted" rather than "it aged out of history".
        let s = compute_attempt_state("ord-X", None, &[], &[], None);
        assert_eq!(s, AttemptState::Unknown);
    }

    #[test]
    fn pending_takes_priority_over_other_branches() {
        // Defensive — the algorithm is ordered, but verify pending
        // wins even if a same-id trade somehow appears in the open
        // list (shouldn't happen, but ordering matters).
        let pending = vec![make_pending("ord-1")];
        let open = vec![make_trade("ord-1", TradeState::Open, 0.0)];
        let s = compute_attempt_state("ord-1", None, &pending, &open, None);
        assert_eq!(s, AttemptState::Pending);
    }

    #[test]
    fn no_match_in_pending_or_open_falls_through() {
        // Different ids, no snapshot → Unknown.
        let pending = vec![make_pending("other-1")];
        let open = vec![make_trade("other-2", TradeState::Open, 0.0)];
        let s = compute_attempt_state("ord-target", None, &pending, &open, None);
        assert_eq!(s, AttemptState::Unknown);
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::*;
    use oanda_client::trades::{Trade, TradeDependentOrder, TradeState};

    fn dependent(price: f64) -> TradeDependentOrder {
        TradeDependentOrder {
            id: "dep".into(),
            r#type: "STOP_LOSS".into(),
            trade_id: "t".into(),
            create_time: String::new(),
            price: Some(price),
            distance: None,
            time_in_force: "GTC".into(),
            trigger_condition: "DEFAULT".into(),
            trigger_mode: None,
            state: "PENDING".into(),
            cancelling_transaction_id: None,
            cancelled_time: None,
        }
    }

    fn trade(id: &str, instrument: &str, units: &str, sl: Option<f64>, tp: Option<f64>) -> Trade {
        Trade {
            id: id.into(),
            instrument: instrument.into(),
            current_units: units.into(),
            price: 1.1,
            open_time: String::new(),
            state: TradeState::Open,
            initial_units: units.into(),
            initial_margin_required: 0.0,
            margin_used: None,
            unrealized_pl: None,
            realized_pl: 0.0,
            average_close_price: None,
            close_time: None,
            closing_transaction_ids: None,
            financing: 0.0,
            dividend_adjustment: 0.0,
            take_profit_order: tp.map(dependent),
            stop_loss_order: sl.map(dependent),
            trailing_stop_loss_order: None,
        }
    }

    fn pending(id: &str, ty: OrderType, units: &str, price: &str) -> PendingOrder {
        PendingOrder {
            id: id.into(),
            r#type: ty,
            instrument: "EUR_USD".into(),
            units: units.into(),
            price: price.into(),
            time_in_force: TimeInForce::Gtc,
            create_time: String::new(),
            take_profit_on_fill: None,
            stop_loss_on_fill: None,
        }
    }

    #[test]
    fn positive_units_is_long_with_sl_tp() {
        let t = trade("t-1", "EUR_USD", "100", Some(1.05), Some(1.20));
        let o = oanda_trade_to_open(&t);
        assert_eq!(
            o,
            OpenPosition {
                instrument: "EUR_USD".into(),
                direction: Direction::Long,
                stop_loss: Some(1.05),
                take_profit: Some(1.20),
                position_id: "t-1".into(),
                order_id: "t-1".into(),
                stake: 100.0,
            }
        );
    }

    #[test]
    fn negative_units_is_short_and_stake_is_absolute() {
        let t = trade("t-2", "USD_JPY", "-250", None, None);
        let o = oanda_trade_to_open(&t);
        assert_eq!(o.direction, Direction::Short);
        assert_eq!(o.stake, 250.0);
        assert_eq!(o.stop_loss, None);
    }

    #[test]
    fn stop_entry_maps_is_stop_true() {
        let p = pending("p-1", OrderType::Stop, "100", "1.1500");
        let m = oanda_order_to_pending(&p).expect("mapped");
        assert_eq!(
            m,
            CorePendingOrder {
                order_id: "p-1".into(),
                instrument: "EUR_USD".into(),
                direction: Direction::Long,
                trigger: 1.1500,
                is_stop: true,
                stake: 100.0,
            }
        );
    }

    #[test]
    fn limit_entry_short_maps_is_stop_false() {
        let p = pending("p-2", OrderType::Limit, "-50", "1.2000");
        let m = oanda_order_to_pending(&p).expect("mapped");
        assert!(!m.is_stop);
        assert_eq!(m.direction, Direction::Short);
        assert_eq!(m.stake, 50.0);
    }

    #[test]
    fn non_entry_order_type_is_skipped() {
        let p = pending("p-3", OrderType::StopLoss, "100", "1.1000");
        assert_eq!(oanda_order_to_pending(&p), None);
    }

    #[test]
    fn unparseable_trigger_is_skipped() {
        let p = pending("p-4", OrderType::Stop, "100", "");
        assert_eq!(oanda_order_to_pending(&p), None);
    }
}

#[cfg(test)]
mod live_flag_tests {
    use super::live_flag_from_secret;

    #[test]
    fn absent_secret_is_practice() {
        assert!(!live_flag_from_secret(None));
    }

    #[test]
    fn true_is_live_case_insensitive() {
        assert!(live_flag_from_secret(Some("true".into())));
        assert!(live_flag_from_secret(Some("TRUE".into())));
        assert!(live_flag_from_secret(Some("True".into())));
    }

    #[test]
    fn anything_else_is_practice() {
        assert!(!live_flag_from_secret(Some("false".into())));
        assert!(!live_flag_from_secret(Some("1".into())));
        assert!(!live_flag_from_secret(Some(String::new())));
    }
}
