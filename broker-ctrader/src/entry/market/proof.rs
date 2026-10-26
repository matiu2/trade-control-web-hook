//! Exact broker identity and fill/protection evidence, never inferred from acceptance.
use super::*;
use std::collections::BTreeSet;

pub(super) fn account(payload: &Value, terms: &Terms) -> Result<()> {
    ensure!(
        integer(payload, "ctidTraderAccountId")? == terms.account,
        "market evidence belongs to another account"
    );
    Ok(())
}

pub(super) fn observed_fill(event: &Value, terms: &Terms) -> Result<i64> {
    ensure!(
        event.get("errorCode").is_none() || is_enum(event, "executionType", 7, "ORDER_REJECTED"),
        "market execution response contains contradictory error state"
    );
    let mut observed = if is_enum(event, "executionType", 3, "ORDER_FILLED") {
        terms.volume
    } else if is_enum(event, "executionType", 11, "ORDER_PARTIAL_FILL") {
        1
    } else {
        0
    };
    for (source, key) in [
        (&event["order"], "executedVolume"),
        (&event["deal"], "filledVolume"),
    ] {
        if source.get(key).is_some() {
            let volume = integer(source, key)?;
            ensure!(
                volume >= 0 && volume <= terms.volume,
                "invalid observed market fill volume"
            );
            observed = observed.max(volume);
        }
    }
    if is_enum(
        &event["position"],
        "positionStatus",
        1,
        "POSITION_STATUS_OPEN",
    ) {
        observed = observed.max(integer(&event["position"]["tradeData"], "volume")?);
    }
    ensure!(
        observed <= terms.volume,
        "market response exceeds requested volume"
    );
    ensure!(
        !(event.get("errorCode").is_some() || is_enum(event, "executionType", 7, "ORDER_REJECTED"))
            || observed == 0,
        "market rejection contradicts observed execution"
    );
    Ok(observed)
}

pub(super) fn rejected_identity(event: &Value, terms: &Terms) -> Result<()> {
    if let Some(client) = event["order"].get("clientOrderId") {
        ensure!(
            client.as_str() == Some(terms.client_id.as_str()),
            "rejected market order belongs to another client ID"
        );
    }
    if event["order"].get("tradeData").is_some() {
        trade_data(&event["order"]["tradeData"], terms, terms.volume)?;
    }
    Ok(())
}

/// Nonterminal snapshots can contain partial execution before order history is complete.
pub(super) fn observed_detail(detail: &Value, terms: &Terms) -> Result<i64> {
    wire::array_shape(detail, "deal")?;
    let declared = if detail["order"].get("executedVolume").is_some() {
        integer(&detail["order"], "executedVolume")?
    } else {
        0
    };
    let deals = array(detail, "deal")
        .iter()
        .try_fold(0_i64, |total, deal| {
            let volume = if deal.get("filledVolume").is_some() {
                integer(deal, "filledVolume")?
            } else {
                0
            };
            ensure!(volume >= 0, "invalid observed market deal volume");
            total
                .checked_add(volume)
                .ok_or_else(|| color_eyre::eyre::eyre!("observed market fill overflow"))
        })?;
    ensure!(
        declared >= 0 && declared <= terms.volume && deals <= terms.volume,
        "observed market detail volume exceeds requested size"
    );
    Ok(declared.max(deals))
}

pub(super) fn trade_data(data: &Value, terms: &Terms, volume: i64) -> Result<()> {
    ensure!(
        integer(data, "symbolId")? == terms.symbol
            && wire::direction(data)? == terms.direction
            && integer(data, "volume")? == volume,
        "market evidence symbol/side/volume mismatch"
    );
    Ok(())
}

pub(super) fn terminal(detail: &Value, terms: &Terms, id: i64) -> Result<Option<i64>> {
    let order = &detail["order"];
    ensure!(
        integer(order, "orderId")? == id
            && order["clientOrderId"].as_str() == Some(terms.client_id.as_str())
            && is_enum(order, "orderType", 1, "MARKET"),
        "market order identity mismatch"
    );
    trade_data(&order["tradeData"], terms, terms.volume)?;
    ensure!(
        order
            .get("closingOrder")
            .is_none_or(|flag| flag.as_bool() == Some(false)),
        "market entry order is a closing order"
    );
    if is_enum(order, "orderStatus", 1, "ORDER_STATUS_ACCEPTED") {
        return Ok(None);
    }
    ensure!(
        [
            (2, "ORDER_STATUS_FILLED"),
            (3, "ORDER_STATUS_REJECTED"),
            (4, "ORDER_STATUS_EXPIRED"),
            (5, "ORDER_STATUS_CANCELLED")
        ]
        .iter()
        .any(|(n, name)| is_enum(order, "orderStatus", *n, name)),
        "unknown market order status"
    );
    wire::array_shape(detail, "deal")?;
    let filled = deal_volume(detail, order, id, terms)?;
    if let Some(declared) = order.get("executedVolume") {
        ensure!(
            integer(&json!({"volume":declared}), "volume")? == filled,
            "market executed volume disagrees with its deals"
        );
    }
    ensure!(
        filled >= 0 && filled <= terms.volume,
        "invalid market executed volume"
    );
    ensure!(
        !is_enum(order, "orderStatus", 2, "ORDER_STATUS_FILLED") || filled == terms.volume,
        "filled market order lacks its full executed volume"
    );
    ensure!(
        !is_enum(order, "orderStatus", 3, "ORDER_STATUS_REJECTED") || filled == 0,
        "market rejection contradicts executed deals"
    );
    Ok(Some(filled))
}

fn deal_volume(detail: &Value, order: &Value, id: i64, terms: &Terms) -> Result<i64> {
    let mut seen = BTreeSet::new();
    array(detail, "deal").iter().try_fold(0_i64, |total, deal| {
        let filled = integer(deal, "filledVolume")?;
        ensure!(
            filled >= 0 && integer(deal, "orderId")? == id,
            "invalid market deal identity/volume"
        );
        ensure!(
            integer(deal, "symbolId")? == terms.symbol
                && wire::direction(deal)? == terms.direction
                && deal.get("closePositionDetail").is_none_or(Value::is_null),
            "market deal has contradictory entry identity"
        );
        if filled == 0 {
            return Ok(total);
        }
        ensure!(
            is_enum(deal, "dealStatus", 2, "FILLED")
                || is_enum(deal, "dealStatus", 3, "PARTIALLY_FILLED"),
            "unconfirmed market deal"
        );
        ensure!(
            integer(deal, "positionId")? == integer(order, "positionId")?
                && integer(deal, "dealId")? > 0
                && seen.insert(integer(deal, "dealId")?)
                && wire::number(deal, "executionPrice")? > 0.0,
            "market deal position/price/identity mismatch"
        );
        total
            .checked_add(filled)
            .ok_or_else(|| color_eyre::eyre::eyre!("market deal volume overflow"))
    })
}
