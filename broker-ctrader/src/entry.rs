use super::*;
use trade_control_core::intent::{Direction, ResolvedEntry, RiskBudget};
use wire::{array, integer, is_enum};
pub(crate) mod geometry;
pub(crate) mod market;
mod sizing;
mod validate;
pub(crate) use geometry::payload;
#[cfg(test)]
pub(crate) use sizing::volume;

#[derive(Default)]
pub(crate) struct SubmissionEvidence {
    pub attempted: bool,
    pub absent: bool,
}
impl SubmissionEvidence {
    pub(crate) fn observe(
        &mut self,
        result: &std::result::Result<Value, ctrader_api::RequestError>,
    ) {
        self.absent = matches!(
            result,
            Err(ctrader_api::RequestError::NotSent(_) | ctrader_api::RequestError::Rejected { .. })
        );
    }
    fn no_risk_added(&self) -> bool {
        !self.attempted || self.absent
    }
}
fn reject(detail: impl Into<String>) -> EntryError {
    EntryError::OrderRejectedWithDetail(detail.into())
}
/// Real market entries require the durable hybrid audit across fill normalization.
pub(crate) fn validate_entry_kind(
    req: &EntryRequest<'_>,
    audited: bool,
) -> std::result::Result<(), EntryError> {
    if matches!(req.entry, ResolvedEntry::Market { .. }) && !req.dry_run && !audited {
        return Err(reject(
            "cTrader market entries require the audited hybrid backend for post-fill absolute SL/TP normalization",
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
    place_with_client_id(broker, cap, max_positions, req, None, |_, _| Ok(())).await
}

pub(crate) async fn place_with_client_id<F>(
    broker: &OpenApiBroker,
    cap: f64,
    max_positions: u32,
    req: &EntryRequest<'_>,
    client_id: Option<&str>,
    mut audit: F,
) -> std::result::Result<Placement, EntryError>
where
    F: FnMut(&str, &Value) -> Result<()> + Send,
{
    validate_entry_kind(req, client_id.is_some())?;
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
    if matches!(req.entry, ResolvedEntry::Market { .. }) {
        market::grid(req, symbol.digits).map_err(|error| reject(error.to_string()))?;
    }
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
    let mut price = geometry::price(req, quote)?;
    let sizing::SizedEntry {
        mut cents,
        implied_pct: implied,
        mut max_stop_distance,
    } = sizing::size(broker, req, &symbol, &trader, price, cap).await?;
    let mut protection = crate::protection::prepare(broker, req, &symbol, cents, cap, client_id)
        .await
        .map_err(|error| reject(format!("prop risk: {error}")))?;
    if let Some(guard) = &protection {
        price = guard.price().map_err(|error| reject(error.to_string()))?;
        cents = guard.cents().map_err(|error| reject(error.to_string()))?;
        max_stop_distance = guard
            .max_stop_distance()
            .map_err(|error| reject(error.to_string()))?;
    }
    let protected_id = protection.as_ref().map(|guard| guard.id().to_owned());
    let client_id = protected_id.as_deref().or(client_id);
    let mut payload = with_client_id(
        payload(broker.account_id(), symbol.id, cents, req, price)?,
        client_id,
    )?;
    if let Some(guard) = &mut protection {
        payload["label"] = json!(format!("risk:{}", guard.setup_id()));
        guard
            .before_submit(broker, req, &symbol)
            .await
            .map_err(|error| reject(format!("prop risk revalidation: {error}")))?;
    }
    if req.dry_run {
        tracing::info!(account=%broker.account_name,instrument=%symbol.name,units=cents as f64/100.0,price,implied_risk_pct=implied,"cTrader dry run; no order submitted");
    }
    let mut submission = SubmissionEvidence::default();
    if matches!(req.entry, ResolvedEntry::Market { .. }) && !req.dry_run {
        let terms = market::Terms {
            account: broker.account_id(),
            symbol: symbol.id,
            volume: cents,
            client_id: client_id
                .ok_or_else(|| reject("market audit ID missing; no order submitted"))?
                .into(),
            direction: req.direction,
            stop: req.stop_loss,
            target: req.take_profit,
            requested_price: price,
            max_stop_distance,
            before: market::before(&snapshot).map_err(|error| reject(error.to_string()))?,
        };
        if let Some(guard) = &mut protection {
            guard.mark_transport();
        }
        let result = market::execute(
            &mut market::Api {
                broker,
                audit: &mut audit,
                submission: &mut submission,
            },
            terms,
            payload,
        )
        .await;
        if matches!(result, Err(EntryError::OrderRejectedWithDetail(_))) {
            submission.absent = true;
        }
        if let Some(guard) = &mut protection {
            guard.outcome(submission.no_risk_added()).map_err(|error| {
                EntryError::AmbiguousSuccess(format!("prop risk outcome persistence: {error}"))
            })?;
        }
        return result;
    }
    if let Some(guard) = &mut protection {
        guard.mark_transport();
    }
    let result = submit(req.dry_run, cents, price, || async {
        submission.attempted = true;
        let response = broker.mutate(2106, payload).await;
        submission.observe(&response);
        response
    })
    .await;
    if let Some(guard) = &mut protection {
        guard.outcome(submission.no_risk_added()).map_err(|error| {
            EntryError::AmbiguousSuccess(format!("prop risk outcome persistence: {error}"))
        })?;
    }
    result
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
