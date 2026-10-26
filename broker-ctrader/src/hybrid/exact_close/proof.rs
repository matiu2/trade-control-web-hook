use super::*;
use crate::wire::{array, integer, is_enum};
use std::collections::BTreeSet;

pub(super) struct Target {
    pub volume: i64,
    pub symbol: i64,
    pub side: i64,
}

pub(super) fn positive_id(raw: &str) -> Result<i64> {
    let id = raw.parse::<i64>()?;
    ensure!(id > 0, "broker ID must be positive");
    Ok(id)
}

pub(super) fn account(value: &Value, expected: i64) -> Result<()> {
    ensure!(
        integer(value, "ctidTraderAccountId")? == expected,
        "exact close account mismatch"
    );
    Ok(())
}

pub(super) fn opening(
    detail: &Value,
    snapshot: &Value,
    account_id: i64,
    order_id: i64,
    position_id: i64,
    expected_client: &str,
) -> Result<Target> {
    account(detail, account_id)?;
    account(snapshot, account_id)?;
    wire::validate_snapshot(snapshot)?;
    let order = &detail["order"];
    ensure!(
        integer(order, "orderId")? == order_id
            && !expected_client.is_empty()
            && order["clientOrderId"].as_str() == Some(expected_client)
            && integer(order, "positionId")? == position_id
            && is_enum(order, "orderStatus", 2, "ORDER_STATUS_FILLED")
            && order["closingOrder"].as_bool() != Some(true),
        "opening order does not prove the exact filled position"
    );
    let matches: Vec<_> = array(snapshot, "position")
        .iter()
        .filter(|p| integer(p, "positionId").ok() == Some(position_id))
        .collect();
    ensure!(
        matches.len() == 1,
        "exact close requires one currently open position"
    );
    let position = matches[0];
    ensure!(
        is_enum(position, "positionStatus", 1, "POSITION_STATUS_OPEN"),
        "target position is not open"
    );
    let target = Target {
        volume: integer(&position["tradeData"], "volume")?,
        symbol: integer(&position["tradeData"], "symbolId")?,
        side: integer(&position["tradeData"], "tradeSide")?,
    };
    ensure!(
        target.volume > 0
            && integer(order, "executedVolume")? == target.volume
            && integer(&order["tradeData"], "volume")? == target.volume
            && integer(&order["tradeData"], "symbolId")? == target.symbol
            && integer(&order["tradeData"], "tradeSide")? == target.side,
        "target position differs from its opening order"
    );
    Ok(target)
}

pub(super) fn closed(
    detail: &Value,
    account_id: i64,
    order_id: i64,
    position_id: i64,
    target: &Target,
) -> Result<bool> {
    account(detail, account_id)?;
    let order = &detail["order"];
    ensure!(
        integer(order, "orderId")? == order_id
            && integer(order, "positionId")? == position_id
            && order["closingOrder"].as_bool() == Some(true)
            && integer(&order["tradeData"], "symbolId")? == target.symbol
            && integer(&order["tradeData"], "tradeSide")? == if target.side == 1 { 2 } else { 1 },
        "closing order does not belong to the exact position"
    );
    if is_enum(order, "orderStatus", 1, "ORDER_STATUS_ACCEPTED") {
        return Ok(false);
    }
    ensure!(
        is_enum(order, "orderStatus", 2, "ORDER_STATUS_FILLED")
            && integer(order, "executedVolume")? == target.volume,
        "full exact position close not confirmed"
    );
    wire::array_shape(detail, "deal")?;
    let mut seen = BTreeSet::new();
    let total = array(detail, "deal").iter().try_fold(0_i64, |sum, deal| {
        ensure!(
            integer(deal, "orderId")? == order_id
                && integer(deal, "positionId")? == position_id
                && integer(deal, "symbolId")? == target.symbol
                && integer(deal, "tradeSide")? == if target.side == 1 { 2 } else { 1 }
                && integer(deal, "dealId")? > 0
                && seen.insert(integer(deal, "dealId")?)
                && is_enum(deal, "dealStatus", 2, "FILLED"),
            "closing deal identity/status mismatch"
        );
        let filled = integer(deal, "filledVolume")?;
        ensure!(
            filled > 0 && integer(&deal["closePositionDetail"], "closedVolume")? == filled,
            "closing deal lacks matching closed volume"
        );
        sum.checked_add(filled)
            .ok_or_else(|| eyre!("closing deal volume overflow"))
    })?;
    ensure!(
        total == target.volume,
        "closing deals do not cover full position volume"
    );
    Ok(true)
}

pub(super) fn absent(snapshot: &Value, account_id: i64, position_id: i64) -> Result<()> {
    account(snapshot, account_id)?;
    wire::validate_snapshot(snapshot)?;
    ensure!(
        !array(snapshot, "position")
            .iter()
            .any(|p| integer(p, "positionId").ok() == Some(position_id)),
        "position remains after closing execution"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
