//! Broker-verified recovery of an existing fill; never submits an entry or changes protection.
use super::*;
use serde::Serialize;
use trade_control_core::{broker_exit::BrokerTradeExit, intent::Direction};

mod broker;
mod evidence;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Serialize)]
pub struct MarketReconciliationProof {
    pub account: String,
    pub account_id: i64,
    pub audit_id: String,
    pub instrument: String,
    pub direction: Direction,
    pub pip_size: f64,
    pub order_id: String,
    pub position_id: String,
    pub units: f64,
    pub entry_price: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub opened_at: DateTime<Utc>,
    pub requested_stop_loss: f64,
    pub requested_take_profit: f64,
    pub audit_intent: Value,
    pub evidence: Value,
    pub closed_exit: Option<BrokerTradeExit>,
}

pub struct MarketReconciliationGuard {
    data: OpenApiBroker,
    lease: fix::reconciliation::Lease,
    proof: MarketReconciliationProof,
}

impl CtraderBroker {
    /// Identify pending audited market entries without clearing their interlocks.
    pub async fn pending_market_audit_ids(&self) -> Result<Vec<String>> {
        self.hybrid_backend()?
            .management
            .pending_market_audit_ids()
            .await
    }

    /// Lock and inspect one existing audited market entry without another broker write.
    pub async fn prepare_market_reconciliation(
        &self,
        audit_id: &str,
    ) -> Result<MarketReconciliationGuard> {
        let hybrid = self.hybrid_backend()?;
        let lease = hybrid.management.reconcile(audit_id).await?;
        let data = OpenApiBroker {
            shared: hybrid.data.shared.clone(),
            account_name: hybrid.data.account_name.clone(),
        };
        let proof = broker::inspect(&data, audit_id, lease.records()).await?;
        Ok(MarketReconciliationGuard { data, lease, proof })
    }
}

impl MarketReconciliationGuard {
    pub fn proof(&self) -> &MarketReconciliationProof {
        &self.proof
    }

    /// Recheck identity and protection while retaining the exclusive account lock.
    pub async fn refresh(&self) -> Result<()> {
        let current =
            broker::inspect(&self.data, &self.proof.audit_id, self.lease.records()).await?;
        evidence::unchanged(&self.proof, &current)
    }

    /// Call only after the matching management state and its receipt were durably committed.
    pub async fn resolve(mut self, durable_receipt: Value) -> Result<()> {
        evidence::receipt(&self.proof, &durable_receipt)?;
        let current =
            broker::inspect(&self.data, &self.proof.audit_id, self.lease.records()).await?;
        evidence::unchanged(&self.proof, &current)?;
        self.lease
            .record("reconciliation-db-committed", &durable_receipt)?;
        self.lease.record(
            "resolved",
            &json!({"confirmed":true,"reconciliation":durable_receipt,"broker_proof":current}),
        )?;
        tracing::info!(account=%self.proof.account,order=%self.proof.order_id,position=%self.proof.position_id,"verified market entry reconciliation completed; no broker mutation submitted");
        Ok(())
    }
}
