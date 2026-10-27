use super::*;
use wire::{array, direction, integer, is_enum, optional_number};

pub(crate) async fn positions(broker: &OpenApiBroker, account: &str) -> Result<Vec<OpenPosition>> {
    ensure!(broker.account_matches(account), "cTrader account mismatch");
    let snapshot = broker.snapshot().await?;
    let symbols = metadata::symbols(broker).await?;
    let mut result = Vec::new();
    for p in array(&snapshot, "position")
        .iter()
        .filter(|p| is_enum(p, "positionStatus", 1, "POSITION_STATUS_OPEN"))
    {
        let position = integer(p, "positionId")?;
        let td = &p["tradeData"];
        let orders = history::position_orders(broker, position, Utc::now()).await?;
        let origin = history::origin(&orders, position)?;
        result.push(OpenPosition {
            instrument: symbol_name(&symbols, integer(td, "symbolId")?)?,
            direction: direction(td)?,
            stop_loss: optional_number(p, "stopLoss"),
            take_profit: optional_number(p, "takeProfit"),
            position_id: position.to_string(),
            order_id: origin
                .map(|o| integer(o, "orderId").map(|v| v.to_string()))
                .transpose()?
                .unwrap_or_default(),
            stake: wire::number(td, "volume")? / 100.0,
            entry_price: optional_number(p, "price"),
            opened_at: integer(td, "openTimestamp")
                .ok()
                .and_then(DateTime::from_timestamp_millis),
        });
    }
    Ok(result)
}
pub(crate) fn symbol_name(symbols: &[Value], id: i64) -> Result<String> {
    symbols
        .iter()
        .find(|s| integer(s, "symbolId").ok() == Some(id))
        .and_then(|s| s["symbolName"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| eyre!("cTrader symbol ID {id} missing from account catalog"))
}
pub(crate) async fn pending(broker: &OpenApiBroker, account: &str) -> Result<Vec<PendingOrder>> {
    ensure!(broker.account_matches(account), "cTrader account mismatch");
    let snapshot = broker.snapshot().await?;
    let symbols = metadata::symbols(broker).await?;
    array(&snapshot, "order")
        .iter()
        .filter(|o| {
            is_enum(o, "orderStatus", 1, "ORDER_STATUS_ACCEPTED")
                && (is_enum(o, "orderType", 2, "LIMIT") || is_enum(o, "orderType", 3, "STOP"))
        })
        .map(|o| {
            let td = &o["tradeData"];
            let stop = is_enum(o, "orderType", 3, "STOP");
            Ok(PendingOrder {
                order_id: integer(o, "orderId")?.to_string(),
                instrument: symbol_name(&symbols, integer(td, "symbolId")?)?,
                direction: direction(td)?,
                trigger: wire::number(o, if stop { "stopPrice" } else { "limitPrice" })?,
                is_stop: stop,
                stake: wire::number(td, "volume")? / 100.0,
            })
        })
        .collect()
}
pub(crate) async fn cancel(broker: &OpenApiBroker, account: &str, id: &str) -> Result<()> {
    ensure!(broker.account_matches(account), "cTrader account mismatch");
    let id: i64 = id.parse()?;
    ensure!(id > 0, "invalid cTrader order ID");
    let event = broker.mutate(2108, json!({"orderId":id})).await?;
    ensure!(
        event.get("errorCode").is_none()
            && is_enum(&event, "executionType", 5, "ORDER_CANCELLED")
            && integer(&event["order"], "orderId")? == id,
        "cTrader did not positively confirm this order's cancellation"
    );
    Ok(())
}
pub(crate) async fn cancel_instrument(broker: &OpenApiBroker, instrument: &str) -> usize {
    let symbol = match metadata::instrument_metadata_id(broker, instrument).await {
        Ok(symbol) => symbol,
        Err(error) => {
            tracing::error!(account=%broker.account_name,instrument,%error,"cTrader cancel pending orders could not resolve symbol; no cancellation submitted");
            return 0;
        }
    };
    let snapshot = match broker.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::error!(account=%broker.account_name,instrument,%error,"cTrader cancel pending orders could not fetch snapshot; no cancellation submitted");
            return 0;
        }
    };
    let mut count = 0;
    for order in array(&snapshot, "order")
        .iter()
        .filter(|o| integer(&o["tradeData"], "symbolId").ok() == Some(symbol))
    {
        match integer(order, "orderId") {
            Ok(id) => match cancel(broker, &broker.account_name, &id.to_string()).await {
                Ok(()) => count += 1,
                Err(error) => {
                    tracing::warn!(account=%broker.account_name,instrument,%error,id,"cTrader cancellation outcome not confirmed")
                }
            },
            Err(error) => {
                tracing::error!(account=%broker.account_name,instrument,%error,"cTrader pending order has malformed ID; cancellation not submitted")
            }
        }
    }
    count
}
pub(crate) async fn close(broker: &OpenApiBroker, instrument: &str) -> CloseOutcome {
    let operation = async {
        let symbol = metadata::instrument_metadata_id(broker, instrument).await?;
        let snapshot = broker.snapshot().await?;
        let positions: Vec<_> = array(&snapshot, "position")
            .iter()
            .filter(|p| {
                integer(&p["tradeData"], "symbolId").ok() == Some(symbol)
                    && is_enum(p, "positionStatus", 1, "POSITION_STATUS_OPEN")
            })
            .collect();
        if positions.is_empty() {
            return Ok(CloseOutcome::NothingOpen);
        }
        for p in &positions {
            let id = integer(p, "positionId")?;
            let volume = integer(&p["tradeData"], "volume")?;
            let event = broker
                .mutate(2111, json!({"positionId":id,"volume":volume}))
                .await?;
            ensure!(
                event.get("errorCode").is_none()
                    && integer(&event["position"], "positionId")? == id
                    && is_enum(
                        &event["position"],
                        "positionStatus",
                        2,
                        "POSITION_STATUS_CLOSED"
                    ),
                "cTrader full close not positively confirmed; reconcile before retry"
            );
        }
        Ok::<_, color_eyre::Report>(CloseOutcome::Closed(positions.len()))
    };
    match operation.await {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(%error,instrument,"cTrader close outcome unconfirmed");
            CloseOutcome::Errored
        }
    }
}
fn preserve(payload: &mut Value, source: &Value, keys: &[&str]) {
    for key in keys {
        if let Some(value) = source.get(*key) {
            payload[*key] = value.clone();
        }
    }
}
pub(crate) fn amend_payload(source: &Value, position: bool, stop: f64) -> Result<Value> {
    ensure!(
        stop.is_finite() && stop > 0.0,
        "invalid cTrader amended stop"
    );
    let key = if position { "positionId" } else { "orderId" };
    let id = integer(source, key)?;
    let mut payload = json!({key:id,"stopLoss":stop});
    preserve(
        &mut payload,
        source,
        &["takeProfit", "guaranteedStopLoss", "trailingStopLoss"],
    );
    if position {
        preserve(&mut payload, source, &["stopLossTriggerMethod"]);
    } else {
        preserve(
            &mut payload,
            source,
            &["limitPrice", "stopPrice", "stopTriggerMethod"],
        );
    }
    Ok(payload)
}
pub(crate) fn resolve_amend<'a>(
    snapshot: &'a Value,
    detail: Option<&Value>,
    id: i64,
) -> Result<Option<(bool, &'a Value)>> {
    let direct = array(snapshot, "position")
        .iter()
        .find(|p| integer(p, "positionId").ok() == Some(id));
    let pending = array(snapshot, "order")
        .iter()
        .find(|o| integer(o, "orderId").ok() == Some(id));
    let linked = detail.and_then(|o| integer(o, "positionId").ok());
    ensure!(
        direct.is_none() || pending.is_none(),
        "ambiguous cTrader ID matches position and pending order"
    );
    if let Some(direct) = direct {
        ensure!(
            linked.is_none_or(|linked| linked == id),
            "ambiguous cTrader position/order numeric ID belongs to different positions"
        );
        return Ok(Some((true, direct)));
    }
    if let Some(pending) = pending {
        return Ok(Some((false, pending)));
    }
    Ok(array(snapshot, "position")
        .iter()
        .find(|p| linked.is_some() && integer(p, "positionId").ok() == linked)
        .map(|p| (true, p)))
}
pub(crate) async fn amend(
    broker: &OpenApiBroker,
    account: &str,
    id: &str,
    stop: f64,
) -> std::result::Result<(), AmendError> {
    if !broker.account_matches(account) || !stop.is_finite() || stop <= 0.0 {
        return Err(AmendError::Transient);
    }
    let id: i64 = id.parse().map_err(|_| AmendError::NotFound)?;
    let operation = async {
        let snapshot = broker.snapshot().await?;
        let detail = match broker.read(2181, 2182, json!({"orderId":id})).await {
            Ok(detail) => {
                ensure!(
                    integer(&detail["order"], "orderId")? == id,
                    "amend history order ID mismatch"
                );
                Some(detail["order"].clone())
            }
            Err(error) if matches!(error.downcast_ref::<ctrader_api::RequestError>(),Some(ctrader_api::RequestError::Rejected {code}) if code=="ORDER_NOT_FOUND") => {
                None
            }
            Err(error) => return Err(error),
        };
        let Some((is_position, source)) = resolve_amend(&snapshot, detail.as_ref(), id)? else {
            return Ok(false);
        };
        let (kind, field) = if is_position {
            (2110, "position")
        } else {
            (2109, "order")
        };
        if crate::protection::enabled(broker)? {
            crate::protection::stop_tightens(source, stop)?;
        }
        let payload = amend_payload(source, is_position, stop)?;
        let response = broker.mutate(kind, payload).await?;
        let actual = &response[field];
        let key = if field == "position" {
            "positionId"
        } else {
            "orderId"
        };
        ensure!(
            response.get("errorCode").is_none()
                && integer(actual, key)? == integer(source, key)?
                && optional_number(actual, "stopLoss").is_some_and(|v| (v - stop).abs() < 1e-9),
            "cTrader stop amendment not confirmed"
        );
        ensure!(
            optional_number(actual, "takeProfit") == optional_number(source, "takeProfit"),
            "cTrader amendment did not preserve take profit"
        );
        Ok::<_, color_eyre::Report>(true)
    };
    match operation.await {
        Ok(true) => Ok(()),
        Ok(false) => Err(AmendError::NotFound),
        Err(error) => {
            tracing::warn!(%error,"cTrader amend outcome unconfirmed");
            Err(AmendError::Transient)
        }
    }
}
