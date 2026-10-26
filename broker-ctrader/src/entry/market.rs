//! Send market entries once, then prove their fill and exact absolute protection.
use super::*;
use std::{collections::BTreeSet, future::Future, time::Duration};
use tokio::time::{sleep, timeout};

pub(crate) mod position;
pub(crate) mod proof;
#[cfg(test)]
mod tests;

pub(crate) struct Terms {
    pub account: i64,
    pub symbol: i64,
    pub volume: i64,
    pub client_id: String,
    pub direction: Direction,
    pub stop: f64,
    pub target: f64,
    pub requested_price: f64,
    pub max_stop_distance: f64,
    pub before: BTreeSet<i64>,
}
enum Completion {
    Filled(Placement),
    Rejected(String),
}

pub(super) trait Transport: Send {
    fn read(
        &mut self,
        kind: u32,
        response: u32,
        payload: Value,
    ) -> impl Future<Output = Result<Value>> + Send;
    fn mutate(
        &mut self,
        kind: u32,
        payload: Value,
    ) -> impl Future<Output = std::result::Result<Value, ctrader_api::RequestError>> + Send;
    fn note(&mut self, phase: &str, evidence: &Value) -> Result<()>;
}

pub(super) struct Api<'a, F> {
    pub broker: &'a OpenApiBroker,
    pub audit: &'a mut F,
}
impl<F> Transport for Api<'_, F>
where
    F: FnMut(&str, &Value) -> Result<()> + Send,
{
    async fn read(&mut self, kind: u32, response: u32, payload: Value) -> Result<Value> {
        self.broker.read(kind, response, payload).await
    }
    async fn mutate(
        &mut self,
        kind: u32,
        payload: Value,
    ) -> std::result::Result<Value, ctrader_api::RequestError> {
        self.broker.mutate(kind, payload).await
    }
    fn note(&mut self, phase: &str, evidence: &Value) -> Result<()> {
        (self.audit)(phase, evidence)
    }
}

pub(super) fn before(snapshot: &Value) -> Result<BTreeSet<i64>> {
    wire::validate_snapshot(snapshot)?;
    array(snapshot, "position")
        .iter()
        .map(|row| integer(row, "positionId"))
        .collect()
}

pub(super) fn grid(req: &EntryRequest<'_>, digits: u32) -> Result<()> {
    let scale = 10_f64.powi(i32::try_from(digits)?);
    ensure!(
        [req.stop_loss, req.take_profit].iter().all(|level| {
            level.is_finite()
                && *level > 0.0
                && (level * scale - (level * scale).round()).abs() < 1e-6
        }),
        "market stop/target must match the native cTrader price grid"
    );
    Ok(())
}

pub(super) async fn execute(
    io: &mut impl Transport,
    terms: Terms,
    payload: Value,
) -> std::result::Result<Placement, EntryError> {
    execute_with_timeout(io, terms, payload, Duration::from_secs(10)).await
}

async fn execute_with_timeout(
    io: &mut impl Transport,
    terms: Terms,
    payload: Value,
    budget: Duration,
) -> std::result::Result<Placement, EntryError> {
    io.note("market-before-send", &json!({"request":payload,"pre_position_ids":terms.before,
        "absolute_stop":terms.stop,"absolute_target":terms.target,"requested_reference_price":terms.requested_price,
        "authorized_max_stop_distance":terms.max_stop_distance}))
        .map_err(|error| reject(format!("market entry not submitted: audit persistence failed: {error}")))?;
    let event = io
        .mutate(2106, payload)
        .await
        .map_err(|error| match error {
            ctrader_api::RequestError::NotSent(reason) => {
                reject(format!("not submitted: {reason}"))
            }
            ctrader_api::RequestError::Rejected { code } => reject(code),
            ctrader_api::RequestError::Unknown(reason) => unknown(&terms, &reason),
        })?;
    let completed = timeout(budget, finalize(io, &terms, &event)).await;
    match completed {
        Ok(Ok(Completion::Filled(placement))) => Ok(placement),
        Ok(Ok(Completion::Rejected(reason))) => Err(reject(reason)),
        Ok(Err(error)) => Err(unknown(&terms, &format!("{error:#}"))),
        Err(_) => Err(unknown(&terms, "fill/protection verification timed out")),
    }
}

fn unknown(terms: &Terms, reason: &str) -> EntryError {
    EntryError::AmbiguousSuccess(format!(
        "cTrader market entry {} may have executed: {reason}; reconcile the durable audit before another mutation",
        terms.client_id
    ))
}

async fn finalize(io: &mut impl Transport, terms: &Terms, event: &Value) -> Result<Completion> {
    proof::account(event, terms)?;
    io.note("market-response", event)?;
    let mut observed = proof::observed_fill(event, terms)?;
    if is_enum(event, "executionType", 7, "ORDER_REJECTED") {
        proof::rejected_identity(event, terms)?;
        return Ok(Completion::Rejected(
            event["errorCode"]
                .as_str()
                .unwrap_or("ORDER_REJECTED")
                .into(),
        ));
    }
    let id = integer(&event["order"], "orderId")?;
    ensure!(id > 0, "market response has invalid order ID");
    let (order, filled) = loop {
        let detail = io.read(2181, 2182, json!({"orderId":id})).await?;
        proof::account(&detail, terms)?;
        io.note("market-order-detail", &detail)?;
        let terminal = proof::terminal(&detail, terms, id)?;
        observed = observed.max(proof::observed_detail(&detail, terms)?);
        if let Some(filled) = terminal {
            ensure!(
                filled >= observed,
                "terminal market detail contradicts observed execution"
            );
            break (detail["order"].clone(), filled);
        }
        sleep(Duration::from_millis(200)).await;
    };
    if filled == 0 {
        return Ok(Completion::Rejected(
            "cTrader market order terminated without any execution".into(),
        ));
    }
    ensure!(
        filled == terms.volume,
        "market IOC partially executed; reconcile its protected position before another mutation"
    );
    ensure!(
        is_enum(&order, "orderStatus", 2, "ORDER_STATUS_FILLED"),
        "executed market order lacks FILLED status; reconcile before another mutation"
    );
    let position_id = integer(&order, "positionId")?;
    ensure!(
        !terms.before.contains(&position_id),
        "market fill points at a pre-existing position"
    );
    let snapshot = io
        .read(2124, 2125, json!({"returnProtectionOrders":false}))
        .await?;
    let position = position::position(&snapshot, terms, id, position_id, filled)?;
    io.note(
        "market-filled-position",
        &json!({"order":order,"position":position}),
    )?;
    position::fill_geometry(position, terms)?;
    let payload = json!({"positionId":position_id,"stopLoss":terms.stop,"takeProfit":terms.target});
    io.note("market-protection-intent", &payload)?;
    let amended = io.mutate(2110, payload).await?;
    proof::account(&amended, terms)?;
    io.note("market-protection-response", &amended)?;
    ensure!(
        amended.get("errorCode").is_none(),
        "cTrader protection amendment rejected"
    );
    position::protected(&amended["position"], terms, position_id, filled)?;
    let verified = io
        .read(2124, 2125, json!({"returnProtectionOrders":false}))
        .await?;
    io.note("market-protection-readback", &verified)?;
    let position = position::position(&verified, terms, id, position_id, filled)?;
    position::protected(position, terms, position_id, filled)?;
    io.note("market-protection-verified", &verified)?;
    Ok(Completion::Filled(Placement {
        order_id: id.to_string(),
        size: Some(filled as f64 / 100.0),
        price: Some(terms.requested_price),
    }))
}
