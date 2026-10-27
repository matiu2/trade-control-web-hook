use super::*;
use crate::{
    entry::market::Terms,
    wire::{integer, number},
};
use std::collections::BTreeSet;

pub(super) struct Context {
    pub terms: Terms,
    pub intent: Value,
    pub instrument: String,
    pub first_at: DateTime<Utc>,
    pub order_id: Option<i64>,
    pub last_position: Value,
}

pub(super) fn context(records: &[Value], account: i64, audit: &str) -> Result<Context> {
    ensure!(
        records
            .iter()
            .all(|r| r["client_order_id"].as_str() == Some(audit)),
        "reconciliation journal client ID mismatch"
    );
    let first = records
        .iter()
        .find(|r| r["phase"] == "intent")
        .ok_or_else(|| eyre!("missing audited intent"))?;
    let intent = first["evidence"]["request"].clone();
    ensure!(
        intent["operation"] == "open-api-entry"
            && intent["entry_kind"] == "market"
            && intent["client_order_id"].as_str() == Some(audit),
        "journal is not this audited market entry"
    );
    let before = records
        .iter()
        .find(|r| r["phase"] == "market-before-send")
        .ok_or_else(|| eyre!("missing durable pre-submission market evidence"))?;
    let pre = &before["evidence"];
    let request = &pre["request"];
    ensure!(
        integer(request, "ctidTraderAccountId")? == account
            && integer(request, "orderType")? == 1
            && request["clientOrderId"].as_str() == Some(audit),
        "market request account/type/client mismatch"
    );
    let direction = crate::wire::direction(request)?;
    ensure!(
        intent["direction"].as_str()
            == Some(if direction == Direction::Long {
                "long"
            } else {
                "short"
            }),
        "market intent direction mismatch"
    );
    let stop = number(&intent, "stop_loss")?;
    let target = number(&intent, "take_profit")?;
    ensure!(
        number(pre, "absolute_stop")? == stop && number(pre, "absolute_target")? == target,
        "audited strategy protection changed"
    );
    let volume = integer(request, "volume")?;
    let max_stop_distance = number(pre, "authorized_max_stop_distance")?;
    let requested_price = number(pre, "requested_reference_price")?;
    ensure!(
        volume > 0 && max_stop_distance > 0.0 && requested_price > 0.0,
        "invalid market sizing evidence"
    );
    let before: BTreeSet<_> = pre["pre_position_ids"]
        .as_array()
        .ok_or_else(|| eyre!("missing pre-position evidence"))?
        .iter()
        .map(|v| integer(&json!({"id":v}), "id"))
        .collect::<Result<_>>()?;
    ensure!(before.iter().all(|id| *id > 0), "invalid pre-position ID");
    let order_id = records
        .iter()
        .rev()
        .filter_map(|r| integer(&r["evidence"]["order"], "orderId").ok())
        .next();
    let last_position = records
        .iter()
        .rev()
        .find(|r| r["phase"] == "market-filled-position")
        .map(|r| r["evidence"]["position"].clone())
        .unwrap_or(Value::Null);
    let instrument = intent["instrument"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| eyre!("missing audited instrument"))?
        .to_owned();
    Ok(Context {
        terms: Terms {
            account,
            symbol: integer(request, "symbolId")?,
            volume,
            client_id: audit.into(),
            direction,
            stop,
            target,
            requested_price,
            before,
            max_stop_distance,
        },
        intent,
        instrument,
        first_at: DateTime::parse_from_rfc3339(
            first["at"]
                .as_str()
                .ok_or_else(|| eyre!("missing audit time"))?,
        )?
        .with_timezone(&Utc),
        order_id,
        last_position,
    })
}

pub(super) fn unchanged(
    old: &MarketReconciliationProof,
    new: &MarketReconciliationProof,
) -> Result<()> {
    ensure!(
        old.account == new.account
            && old.account_id == new.account_id
            && old.audit_id == new.audit_id
            && old.order_id == new.order_id
            && old.position_id == new.position_id
            && old.instrument == new.instrument
            && old.direction == new.direction
            && old.units == new.units
            && old.entry_price == new.entry_price
            && old.stop_loss == new.stop_loss
            && old.take_profit == new.take_profit
            && (old.closed_exit.is_none() || new.closed_exit.is_some()),
        "broker position/protection changed during reconciliation; preview again"
    );
    Ok(())
}

pub(super) fn receipt(proof: &MarketReconciliationProof, value: &Value) -> Result<()> {
    ensure!(
        value["committed"] == true
            && value["version"] == 1
            && value["operation"] == "keep-protection-attach"
            && value["trade_id"].as_str().is_some_and(|s| !s.is_empty())
            && value["account"].as_str() == Some(proof.account.as_str())
            && integer(value, "account_id")? == proof.account_id
            && value["audit_id"].as_str() == Some(proof.audit_id.as_str())
            && value["order_id"].as_str() == Some(proof.order_id.as_str())
            && value["position_id"].as_str() == Some(proof.position_id.as_str())
            && number(value, "stop_loss")? == proof.stop_loss
            && number(value, "take_profit")? == proof.take_profit
            && number(value, "entry_price")? == proof.entry_price
            && number(value, "units")? == proof.units,
        "reconciliation receipt does not prove the matching management state was committed"
    );
    Ok(())
}
