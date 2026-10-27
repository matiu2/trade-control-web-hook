use super::*;
use crate::{
    entry::market::{Terms, position, proof},
    wire::{array, integer, number},
};
use std::time::Duration;

pub(super) async fn inspect(
    data: &OpenApiBroker,
    audit: &str,
    records: &[Value],
) -> Result<MarketReconciliationProof> {
    tokio::time::timeout(Duration::from_secs(20), inspect_inner(data, audit, records))
        .await
        .map_err(|_| {
            eyre!("market reconciliation read deadline expired; no broker mutation sent")
        })?
}

async fn inspect_inner(
    data: &OpenApiBroker,
    audit: &str,
    records: &[Value],
) -> Result<MarketReconciliationProof> {
    let context = evidence::context(records, data.account_id(), audit)?;
    let symbol = data.instrument_metadata(&context.instrument).await?;
    ensure!(
        symbol.id == context.terms.symbol && symbol.name == context.instrument,
        "broker instrument identity differs from audited entry"
    );
    let order_id = match context.order_id {
        Some(id) => id,
        None => find_order(data, &context).await?,
    };
    let detail = data.read(2181, 2182, json!({"orderId":order_id})).await?;
    proof::account(&detail, &context.terms)?;
    let filled = proof::terminal(&detail, &context.terms, order_id)?.ok_or_else(|| {
        eyre!("market order is not terminal; keep its audit interlock and inspect again")
    })?;
    ensure!(
        filled == context.terms.volume
            && crate::wire::is_enum(&detail["order"], "orderStatus", 2, "ORDER_STATUS_FILLED"),
        "market reconciliation requires a proven full fill; never infer an entry from absence"
    );
    let position_id = integer(&detail["order"], "positionId")?;
    ensure!(
        position_id > 0 && !context.terms.before.contains(&position_id),
        "market entry refers to a pre-existing position"
    );
    let snapshot = data.snapshot().await?;
    let open = array(&snapshot, "position")
        .iter()
        .any(|p| integer(p, "positionId").ok() == Some(position_id));
    let (current, exit) = if open {
        (
            position::position(&snapshot, &context.terms, order_id, position_id, filled)?.clone(),
            None,
        )
    } else {
        let exit=data.lookup_trade_exit(&context.instrument,&order_id.to_string(),Some(&position_id.to_string())).await
            .map_err(|error|eyre!("closed position history unavailable: {error:?}"))?
            .ok_or_else(||eyre!("position absent without confirmed closed history; reconciliation remains blocked"))?;
        ensure!(
            exit.broker_trade_id == position_id.to_string(),
            "closed history position identity mismatch"
        );
        ensure!(
            integer(&context.last_position, "positionId")? == position_id,
            "closed fill lacks its audited protection snapshot"
        );
        (context.last_position.clone(), Some(exit))
    };
    prove(data, &context, &symbol, &detail, &snapshot, &current, exit)
}

fn prove(
    data: &OpenApiBroker,
    context: &evidence::Context,
    symbol: &InstrumentMetadata,
    detail: &Value,
    snapshot: &Value,
    current: &Value,
    exit: Option<BrokerTradeExit>,
) -> Result<MarketReconciliationProof> {
    let id = integer(current, "positionId")?;
    let stop = number(current, "stopLoss")?;
    let target = number(current, "takeProfit")?;
    let accepted = Terms {
        account: context.terms.account,
        symbol: context.terms.symbol,
        volume: context.terms.volume,
        client_id: context.terms.client_id.clone(),
        direction: context.terms.direction,
        stop,
        target,
        requested_price: context.terms.requested_price,
        before: context.terms.before.clone(),
        max_stop_distance: context.terms.max_stop_distance,
    };
    position::protected(current, &accepted, id, context.terms.volume)?;
    let opened = integer(&current["tradeData"], "openTimestamp")?;
    let opened_at = DateTime::from_timestamp_millis(opened)
        .ok_or_else(|| eyre!("invalid broker opening time"))?;
    Ok(MarketReconciliationProof {
        account: data.account_name.clone(),
        account_id: data.account_id(),
        audit_id: context.terms.client_id.clone(),
        instrument: context.instrument.clone(),
        direction: context.terms.direction,
        pip_size: symbol.pip_size,
        order_id: integer(&detail["order"], "orderId")?.to_string(),
        position_id: id.to_string(),
        units: context.terms.volume as f64 / 100.0,
        entry_price: number(current, "price")?,
        stop_loss: stop,
        take_profit: target,
        opened_at,
        requested_stop_loss: context.terms.stop,
        requested_take_profit: context.terms.target,
        audit_intent: context.intent.clone(),
        evidence: json!({"order_detail":detail,"snapshot":snapshot,"accepted_position":current,"authorized_max_stop_distance":context.terms.max_stop_distance}),
        closed_exit: exit,
    })
}

async fn find_order(data: &OpenApiBroker, context: &evidence::Context) -> Result<i64> {
    let from = context.first_at.timestamp_millis().saturating_sub(5000);
    let mut end = Utc::now().timestamp_millis();
    ensure!(
        end >= from && end - from <= 7 * 24 * 3600 * 1000,
        "audit is outside the bounded order-history recovery window"
    );
    let mut found = std::collections::BTreeSet::new();
    for _page in 0..100 {
        let result = data
            .read(2175, 2176, json!({"fromTimestamp":from,"toTimestamp":end}))
            .await?;
        ensure!(
            integer(&result, "ctidTraderAccountId")? == data.account_id(),
            "order recovery account mismatch"
        );
        crate::wire::array_shape(&result, "order")?;
        let rows = array(&result, "order");
        for row in rows
            .iter()
            .filter(|r| r["clientOrderId"].as_str() == Some(context.terms.client_id.as_str()))
        {
            found.insert(integer(row, "orderId")?);
        }
        if result["hasMore"].as_bool() == Some(false) {
            ensure!(
                found.len() == 1,
                "audit client ID has no unique broker order; keep the interlock"
            );
            return found
                .first()
                .copied()
                .ok_or_else(|| eyre!("market order recovery found no order"));
        }
        ensure!(
            result["hasMore"].as_bool() == Some(true),
            "order recovery pagination state unavailable"
        );
        let oldest = rows
            .iter()
            .map(|r| integer(r, "utcLastUpdateTimestamp"))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .min()
            .ok_or_else(|| eyre!("order recovery pagination stalled"))?;
        ensure!(
            oldest < end && oldest >= from,
            "order recovery pagination stalled"
        );
        end = oldest;
    }
    Err(eyre!(
        "order recovery exceeded bounded pages; keep the audit interlock"
    ))
}
