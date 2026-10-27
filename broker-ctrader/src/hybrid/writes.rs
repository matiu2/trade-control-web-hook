//! Open API writes share FIX's durable account interlock.
use super::*;
use trade_control_core::intent::{ResolvedEntry, RiskBudget};

impl Hybrid {
    pub(crate) async fn place_entry(
        &self,
        cap: f64,
        positions: u32,
        req: &EntryRequest<'_>,
    ) -> std::result::Result<Placement, EntryError> {
        if req.dry_run {
            return entry::place(&self.data, cap, positions, req).await;
        }
        entry::validate_entry_kind(req, true)?;
        let mut guard = self
            .management
            .companion_write("open-api-entry", entry_intent(cap, positions, req))
            .await
            .map_err(|error| {
                EntryError::OrderRejectedWithDetail(format!("Open API entry not sent: {error}"))
            })?;
        let id = guard.id().to_owned();
        let result = entry::place_with_client_id(
            &self.data,
            cap,
            positions,
            req,
            Some(&id),
            |phase, value| guard.note(phase, value),
        )
        .await;
        let (receipt, confirmed) = entry_receipt(&id, &result);
        if let Err(error) = guard.record(&receipt, confirmed) {
            return match result {
                Ok(placement) => Err(EntryError::AmbiguousSuccess(format!(
                    "Open API order {} was confirmed, but audit persistence failed for {id}: {error}; reconcile before another mutation",
                    placement.order_id
                ))),
                Err(EntryError::AmbiguousSuccess(detail)) => Err(EntryError::AmbiguousSuccess(
                    format!("{detail}; audit persistence failed for {id}: {error}"),
                )),
                Err(error) => Err(error),
            };
        }
        result
    }

    pub(crate) async fn amend_stop(
        &self,
        account: &str,
        id: &str,
        stop: f64,
    ) -> std::result::Result<(), AmendError> {
        if !self.data.account_matches(account) || !stop.is_finite() || stop <= 0.0 {
            return Err(AmendError::Transient);
        }
        if !id.parse::<i64>().is_ok_and(|id| id > 0) {
            return Err(AmendError::NotFound);
        }
        let mut guard = self.management.companion_write("open-api-amend-stop", json!({"position_or_order_id":id,"new_stop":stop})).await
            .map_err(|error| { tracing::warn!(%error,"Open API stop amendment not sent; account reconciliation required"); AmendError::Transient })?;
        let result = self.data.amend_stop(account, id, stop).await;
        let confirmed = amendment_confirmed(&result);
        let receipt = json!({"position_or_order_id":id,"new_stop":stop,"confirmed":confirmed,"result":result.as_ref().err().map(ToString::to_string)});
        guard.record(&receipt, confirmed).map_err(|error| {
            tracing::error!(%error,"Open API stop amendment audit failed; reconcile before another mutation");
            AmendError::Transient
        })?;
        result
    }
}

fn entry_intent(cap: f64, positions: u32, req: &EntryRequest<'_>) -> Value {
    let kind = match req.entry {
        ResolvedEntry::Market { .. } => "market",
        ResolvedEntry::Stop { .. } => "stop",
        ResolvedEntry::Limit { .. } => "limit",
    };
    let (risk_kind, risk_value) = match req.risk {
        RiskBudget::Percent(v) => ("percent", v),
        RiskBudget::Amount(v) => ("amount", v),
        RiskBudget::Units(v) => ("units", v),
    };
    json!({"instrument":req.instrument,"direction":req.direction,"entry_kind":kind,"entry_price":req.entry.reference_price(),"stop_loss":req.stop_loss,"take_profit":req.take_profit,"risk_kind":risk_kind,"risk_value":risk_value,"max_risk_pct":cap,"max_positions":positions})
}

fn entry_receipt(id: &str, result: &std::result::Result<Placement, EntryError>) -> (Value, bool) {
    match result {
        Ok(p) => (
            json!({"client_order_id":id,"outcome":"accepted","order_id":p.order_id,"size":p.size,"requested_price":p.price}),
            true,
        ),
        Err(EntryError::AmbiguousSuccess(detail)) => (
            json!({"client_order_id":id,"outcome":"unknown","reason":detail}),
            false,
        ),
        Err(error) => (
            json!({"client_order_id":id,"outcome":"not_placed","reason":error.to_string()}),
            true,
        ),
    }
}

fn amendment_confirmed(result: &std::result::Result<(), AmendError>) -> bool {
    matches!(result, Ok(()) | Err(AmendError::NotFound))
}

#[cfg(test)]
mod tests;
