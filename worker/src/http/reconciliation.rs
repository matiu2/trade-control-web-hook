//! Enrich ambiguous HTTP replies with read-only evidence, retaining write interlocks.
use std::time::Duration;

use axum::http::StatusCode;
use broker_ctrader::{CtraderBroker, reconciliation::MarketReconciliationProof};
use color_eyre::eyre::{Result, ensure, eyre};
use trade_control_core::{
    dispatch::ActionResult,
    dispatch_config::DispatchConfig,
    incoming::Verified,
    intent::{Action, BrokerKind, EntrySpec, Resolved, RiskBudget},
};

#[cfg(test)]
mod tests;

pub(super) async fn response(
    broker: &CtraderBroker,
    verified: &Verified,
    cfg: &DispatchConfig,
    result: &ActionResult,
) -> Option<(StatusCode, String)> {
    let ActionResult::AmbiguousSuccess(outcome) = result else {
        return None;
    };
    if !ordinary_market(verified) {
        return None;
    }
    match tokio::time::timeout(
        Duration::from_secs(20),
        inspect(broker, verified, cfg, outcome),
    )
    .await
    {
        Ok(Ok(parts)) => Some(parts),
        Ok(Err(error)) => {
            tracing::warn!(intent_id=%verified.intent.id, %error, "ambiguous entry remains unconfirmed; read-only fill proof unavailable");
            None
        }
        Err(_) => {
            tracing::warn!(intent_id=%verified.intent.id, "read-only entry reconciliation timed out; write interlock retained");
            None
        }
    }
}

async fn inspect(
    broker: &CtraderBroker,
    verified: &Verified,
    cfg: &DispatchConfig,
    outcome: &str,
) -> Result<(StatusCode, String)> {
    let pending = broker.pending_market_audit_ids().await?;
    let audit_id = candidate(&pending, outcome)
        .ok_or_else(|| eyre!("no unique pending audit ID bound to this entry outcome"))?;
    let guard = broker.prepare_market_reconciliation(audit_id).await?;
    let proof = guard.proof();
    ensure!(proof.audit_id == audit_id, "broker proof audit ID mismatch");
    matches_request(verified, cfg, broker.account_id(), proof)?;
    tracing::warn!(intent_id=%verified.intent.id, order=%proof.order_id, position=%proof.position_id, "broker proved filled entry; management and audit reconciliation still required");
    // Dropping this read-only guard deliberately retains the unresolved journal.
    Ok(filled_response(proof))
}

fn ordinary_market(verified: &Verified) -> bool {
    let intent = &verified.intent;
    intent.action == Action::Enter
        && intent.broker == BrokerKind::Ctrader
        && matches!(intent.entry, Some(EntrySpec::Market))
        && intent.mw.is_none()
        && intent.dry_run != Some(true)
}

fn candidate<'a>(pending: &'a [String], outcome: &str) -> Option<&'a str> {
    let [audit_id] = pending else {
        return None;
    };
    if audit_id.is_empty() {
        return None;
    }
    let token_character =
        |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':');
    outcome
        .match_indices(audit_id)
        .any(|(offset, _)| {
            outcome[..offset]
                .chars()
                .next_back()
                .is_none_or(|ch| !token_character(ch))
                && outcome[offset + audit_id.len()..]
                    .chars()
                    .next()
                    .is_none_or(|ch| !token_character(ch))
        })
        .then_some(audit_id)
}

fn matches_request(
    verified: &Verified,
    cfg: &DispatchConfig,
    account_id: Option<i64>,
    proof: &MarketReconciliationProof,
) -> Result<()> {
    let intent = &verified.intent;
    ensure!(
        ordinary_market(verified),
        "request is not an ordinary cTrader market entry"
    );
    ensure!(
        account_id == Some(proof.account_id)
            && proof.account_id > 0
            && intent.account.as_deref() == Some(proof.account.as_str())
            && intent.instrument == proof.instrument
            && intent.direction == Some(proof.direction),
        "broker proof does not match current request identity"
    );
    let pip = intent.pip_size.unwrap_or(cfg.pip_size);
    let tick = intent.tick_size.or(cfg.tick_size).unwrap_or(pip);
    ensure!(
        same(pip, proof.pip_size),
        "broker pip convention differs from request"
    );
    let resolved = Resolved::from_intent(intent, &verified.shell, pip, tick)
        .map_err(|error| eyre!("current request could not be resolved: {error:?}"))?;
    ensure!(
        !resolved.dry_run && resolved.contract_multiplier.is_none(),
        "unsupported market request"
    );
    let audit = &proof.audit_intent;
    ensure!(
        audit["instrument"].as_str() == Some(proof.instrument.as_str())
            && audit["direction"] == serde_json::to_value(proof.direction)?
            && audit["entry_kind"].as_str() == Some("market"),
        "broker audited identity differs from request"
    );
    let (risk_kind, risk_value) = match resolved.risk {
        RiskBudget::Percent(value) => ("percent", value),
        RiskBudget::Amount(value) => ("amount", value),
        RiskBudget::Units(value) => ("units", value),
    };
    ensure!(
        audit["risk_kind"].as_str() == Some(risk_kind),
        "broker risk mode differs from request"
    );
    for (field, expected) in [
        ("entry_price", resolved.entry.reference_price()),
        ("stop_loss", resolved.stop_loss),
        ("take_profit", resolved.take_profit),
        ("risk_value", risk_value),
        ("max_risk_pct", cfg.effective_max_risk_pct()),
        (
            "max_positions",
            f64::from(cfg.effective_max_open_positions()),
        ),
    ] {
        ensure!(
            audit[field]
                .as_f64()
                .is_some_and(|actual| same(actual, expected)),
            "broker audit {field} differs from current request"
        );
    }
    ensure!(
        same(resolved.stop_loss, proof.requested_stop_loss)
            && same(resolved.take_profit, proof.requested_take_profit),
        "requested protection differs from current request"
    );
    Ok(())
}

fn same(left: f64, right: f64) -> bool {
    left.is_finite()
        && right.is_finite()
        && (left - right).abs() <= 1e-10 * left.abs().max(right.abs()).max(1.0)
}

fn filled_response(proof: &MarketReconciliationProof) -> (StatusCode, String) {
    let mut body = format!(
        "filled, needs reconciliation: order={} position={} units={} fill={} SL={} TP={}",
        proof.order_id,
        proof.position_id,
        proof.units,
        proof.entry_price,
        proof.stop_loss,
        proof.take_profit
    );
    if !same(proof.stop_loss, proof.requested_stop_loss)
        || !same(proof.take_profit, proof.requested_take_profit)
    {
        body.push_str(&format!("; broker protection differs from requested SL={} / requested TP={}; requested normalization was not confirmed", proof.requested_stop_loss, proof.requested_take_profit));
    }
    if let Some(exit) = &proof.closed_exit {
        body.push_str("; broker confirms position closed");
        if let Some(price) = exit.exit_price {
            body.push_str(&format!(" exit={price}"));
        }
    } else {
        body.push_str("; server management is not attached");
    }
    body.push_str("; do not retry; reconcile the original request");
    (StatusCode::CONFLICT, body)
}
