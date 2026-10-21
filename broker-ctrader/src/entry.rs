use super::*;
use trade_control_core::intent::{Direction, ResolvedEntry, RiskBudget};
use wire::{array, integer, is_enum};
mod geometry;
mod sizing;
mod validate;
pub(crate) use geometry::payload;
#[cfg(test)]
pub(crate) use sizing::volume;

fn reject(detail: impl Into<String>) -> EntryError {
    EntryError::OrderRejectedWithDetail(detail.into())
}
/// The core tracks absolute strategy protection. Relative market protection shifts
/// with slippage; refuse until actual fills can be normalized and durably tracked.
pub(crate) fn validate_entry_kind(req: &EntryRequest<'_>) -> std::result::Result<(), EntryError> {
    if matches!(req.entry, ResolvedEntry::Market { .. }) {
        return Err(reject(
            "cTrader market entries are unavailable until post-fill absolute SL/TP normalization is verified; use stop or limit entries",
        ));
    }
    Ok(())
}
pub(crate) async fn place(
    broker: &OpenApiBroker,
    cap: f64,
    max_positions: u32,
    req: &EntryRequest<'_>,
) -> std::result::Result<Placement, EntryError> {
    place_with_client_id(broker, cap, max_positions, req, None).await
}

pub(crate) async fn place_with_client_id(
    broker: &OpenApiBroker,
    cap: f64,
    max_positions: u32,
    req: &EntryRequest<'_>,
    client_id: Option<&str>,
) -> std::result::Result<Placement, EntryError> {
    validate_entry_kind(req)?;
    if !cap.is_finite() || cap <= 0.0 {
        return Err(reject("risk cap must be finite and positive"));
    }
    if req.contract_multiplier.is_some() {
        return Err(reject(
            "cTrader adapter currently supports spot FX unit contracts only",
        ));
    }
    let symbol = broker
        .instrument_metadata(req.instrument)
        .await
        .map_err(|e| reject(e.to_string()))?;
    let trader = validate::account(broker, req, &symbol).await?;
    let snapshot = broker
        .snapshot()
        .await
        .map_err(|_| EntryError::AccountFetch)?;
    if array(&snapshot, "position").len() + array(&snapshot, "order").len()
        >= max_positions as usize
    {
        return Err(EntryError::OpenPositionsCapExceeded);
    }
    let quote = metadata::quote(broker, &symbol.name)
        .await
        .map_err(|_| EntryError::AccountFetch)?;
    let price = geometry::price(req, quote)?;
    let (cents, implied) = sizing::size(broker, req, &symbol, &trader, price, cap).await?;
    let payload = with_client_id(
        payload(broker.account_id(), symbol.id, cents, req, price)?,
        client_id,
    )?;
    if req.dry_run {
        tracing::info!(account=%broker.account_name,instrument=%symbol.name,units=cents as f64/100.0,price,implied_risk_pct=implied,"cTrader dry run; no order submitted");
    }
    submit(req.dry_run, cents, price, || broker.mutate(2106, payload)).await
}

pub(crate) async fn submit<F, Fut>(
    dry_run: bool,
    cents: i64,
    price: f64,
    send: F,
) -> std::result::Result<Placement, EntryError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<Value, ctrader_api::RequestError>>,
{
    if dry_run {
        return Ok(Placement {
            order_id: "dry-run".into(),
            size: Some(cents as f64 / 100.0),
            price: Some(price),
        });
    }
    let event = send().await.map_err(|error| match error {
        ctrader_api::RequestError::Rejected { code } => reject(code),
        ctrader_api::RequestError::NotSent(detail) => reject(format!("not submitted: {detail}")),
        ctrader_api::RequestError::Unknown(detail) => EntryError::AmbiguousSuccess(detail),
    })?;
    response(&event, cents, price)
}

pub(crate) fn response(
    event: &Value,
    cents: i64,
    price: f64,
) -> std::result::Result<Placement, EntryError> {
    if event.get("errorCode").is_some() || is_enum(event, "executionType", 7, "ORDER_REJECTED") {
        return Err(reject(
            event["errorCode"].as_str().unwrap_or("ORDER_REJECTED"),
        ));
    }
    if ![2, 3, 11]
        .iter()
        .any(|n| integer(event, "executionType").ok() == Some(*n))
        && !["ORDER_ACCEPTED", "ORDER_FILLED", "ORDER_PARTIAL_FILL"]
            .iter()
            .any(|n| event["executionType"].as_str() == Some(n))
    {
        return Err(EntryError::AmbiguousSuccess(
            "unexpected cTrader execution response; reconcile before retry".into(),
        ));
    }
    let order = integer(&event["order"], "orderId").map_err(|_| {
        EntryError::AmbiguousSuccess("accepted cTrader order response omitted order ID".into())
    })?;
    Ok(Placement {
        order_id: order.to_string(),
        size: Some(cents as f64 / 100.0),
        price: Some(price),
    })
}

pub(crate) fn with_client_id(
    mut payload: Value,
    client_id: Option<&str>,
) -> std::result::Result<Value, EntryError> {
    if let Some(id) = client_id {
        if id.is_empty()
            || id.len() > 50
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        {
            return Err(reject(
                "Open API client order ID must contain 1–50 safe ASCII characters",
            ));
        }
        payload["clientOrderId"] = json!(id);
    }
    Ok(payload)
}
