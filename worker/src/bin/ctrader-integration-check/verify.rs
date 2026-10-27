use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};
use serde_json::Value;
use std::collections::BTreeSet;
use trade_control_core::{broker::EntryRequest, intent::ResolvedEntry};

pub fn integer(value: &Value, key: &str) -> Result<i64> {
    value
        .get(key)
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .ok_or_else(|| eyre!("missing integer {key}"))
}
pub fn rows<'a>(value: &'a Value, key: &str) -> Result<&'a [Value]> {
    match value.get(key) {
        None => Ok(&[]),
        Some(v) => v
            .as_array()
            .map(Vec::as_slice)
            .ok_or_else(|| eyre!("invalid array {key}")),
    }
}
pub fn ids(value: &Value, key: &str, id: &str) -> Result<BTreeSet<i64>> {
    rows(value, key)?.iter().map(|r| integer(r, id)).collect()
}
pub fn own(id: i64, baseline: &BTreeSet<i64>) -> Result<()> {
    ensure!(
        id > 0 && !baseline.contains(&id),
        "probe ID belongs to pre-existing broker state; no mutation allowed"
    );
    Ok(())
}

pub fn client_identity(detail: &Value) -> Result<&str> {
    let client = detail["order"]["clientOrderId"]
        .as_str()
        .ok_or_else(|| eyre!("probe client order ID missing"))?;
    ensure!(
        client.starts_with(&format!("manage-{}-", std::process::id())),
        "order client ID does not belong to this probe process"
    );
    Ok(client)
}

pub fn pending_geometry(detail: &Value, req: &EntryRequest<'_>) -> Result<()> {
    let order = &detail["order"];
    client_identity(detail)?;
    let (kind, key, price) = match req.entry {
        ResolvedEntry::Stop { trigger_price } => (3, "stopPrice", trigger_price),
        ResolvedEntry::Limit { trigger_price } => (2, "limitPrice", trigger_price),
        ResolvedEntry::Market { .. } => return Err(eyre!("market order used in pending probe")),
    };
    ensure!(
        integer(order, "orderType")? == kind && integer(&order["tradeData"], "tradeSide")? == 1,
        "pending probe kind/direction mismatch"
    );
    for (key, expected) in [
        (key, price),
        ("stopLoss", req.stop_loss),
        ("takeProfit", req.take_profit),
    ] {
        ensure!(
            order[key]
                .as_f64()
                .is_some_and(|value| (value - expected).abs() < 1e-9),
            "pending probe {key} mismatch"
        );
    }
    Ok(())
}

pub fn pending(
    detail: &Value,
    snapshot: &Value,
    order_id: i64,
    symbol_id: i64,
    volume: i64,
) -> Result<()> {
    let o = &detail["order"];
    ensure!(
        integer(o, "orderId")? == order_id
            && integer(o, "orderStatus")? == 1
            && integer(&o["tradeData"], "symbolId")? == symbol_id
            && integer(&o["tradeData"], "volume")? == volume,
        "pending probe order identity/status/volume mismatch"
    );
    ensure!(
        rows(snapshot, "order")?
            .iter()
            .filter(|o| integer(o, "orderId").ok() == Some(order_id))
            .count()
            == 1,
        "pending probe is not uniquely present"
    );
    Ok(())
}
pub fn cancelled(detail: &Value, snapshot: &Value, order_id: i64) -> Result<()> {
    let o = &detail["order"];
    ensure!(
        integer(o, "orderId")? == order_id
            && integer(o, "orderStatus")? == 5
            && match o.get("executedVolume") {
                None => true,
                Some(_) => integer(o, "executedVolume")? == 0,
            },
        "probe cancellation is not positively confirmed without execution"
    );
    ensure!(
        rows(detail, "deal")?
            .iter()
            .all(|d| integer(d, "filledVolume").ok() == Some(0)),
        "cancelled probe has execution deals"
    );
    ensure!(
        !rows(snapshot, "order")?
            .iter()
            .any(|o| integer(o, "orderId").ok() == Some(order_id)),
        "cancelled probe remains pending"
    );
    Ok(())
}
pub fn filled_position(
    detail: &Value,
    snapshot: &Value,
    order_id: i64,
    symbol_id: i64,
    volume: i64,
    stop: f64,
    target: f64,
) -> Result<i64> {
    let o = &detail["order"];
    client_identity(detail)?;
    ensure!(
        integer(o, "orderId")? == order_id
            && integer(o, "orderStatus")? == 2
            && integer(o, "executedVolume")? == volume,
        "market probe has no full broker fill"
    );
    ensure!(
        integer(&o["tradeData"], "symbolId")? == symbol_id
            && integer(&o["tradeData"], "volume")? == volume
            && integer(&o["tradeData"], "tradeSide")? == 1,
        "market opening order symbol/volume/direction mismatch"
    );
    let pid = integer(o, "positionId")?;
    let p = rows(snapshot, "position")?
        .iter()
        .find(|p| integer(p, "positionId").ok() == Some(pid))
        .ok_or_else(|| eyre!("market probe position missing"))?;
    ensure!(
        integer(p, "positionStatus")? == 1
            && integer(&p["tradeData"], "symbolId")? == symbol_id
            && integer(&p["tradeData"], "volume")? == volume,
        "market position identity/volume mismatch"
    );
    ensure!(
        p["stopLoss"]
            .as_f64()
            .is_some_and(|s| (s - stop).abs() < 1e-9)
            && p["takeProfit"]
                .as_f64()
                .is_some_and(|t| (t - target).abs() < 1e-9),
        "market probe absolute protection mismatch"
    );
    Ok(pid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn existing_ids_and_absence_only_cancellation_are_rejected() {
        crate::test_logging();
        assert!(own(1, &BTreeSet::from([1])).is_err());
        own(2, &BTreeSet::from([1])).unwrap();
        let s = json!({"order":[],"position":[]});
        let d = json!({"order":{"orderId":2,"orderStatus":1,"executedVolume":0},"deal":[]});
        assert!(cancelled(&d, &s, 2).is_err());
        let d = json!({"order":{"orderId":2,"orderStatus":5,"executedVolume":0},"deal":[]});
        cancelled(&d, &s, 2).unwrap();
        let d = json!({"order":{"orderId":2,"orderStatus":5,"executedVolume":10},"deal":[]});
        assert!(cancelled(&d, &s, 2).is_err());
    }

    #[test]
    fn pending_read_must_match_probe_client_and_absolute_protection() {
        crate::test_logging();
        let req = EntryRequest {
            setup_id: None,
            instrument: "EURUSD",
            direction: trade_control_core::intent::Direction::Long,
            entry: ResolvedEntry::Stop {
                trigger_price: 1.15,
            },
            stop_loss: 1.14,
            take_profit: 1.165,
            risk: trade_control_core::intent::RiskBudget::Units(1000.0),
            dry_run: false,
            contract_multiplier: None,
        };
        let mut detail = json!({"order":{"clientOrderId":format!("manage-{}-test",std::process::id()),"orderType":3,"tradeData":{"tradeSide":1},"stopPrice":1.15,"stopLoss":1.14,"takeProfit":1.165}});
        pending_geometry(&detail, &req).unwrap();
        detail["order"]["stopLoss"] = json!(1.13);
        assert!(pending_geometry(&detail, &req).is_err());
        detail["order"]["stopLoss"] = json!(1.14);
        detail["order"]["clientOrderId"] = json!("user-order");
        assert!(pending_geometry(&detail, &req).is_err());
    }
}
