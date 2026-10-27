//! Attach a proven cTrader fill without submitting or changing broker orders.
//!
//! The operator must explicitly approve the current broker protection. Recovery
//! authenticates the original webhook at its receipt time and commits tracking,
//! replay protection and audit evidence together before resolving broker doubt.
mod request;
mod storage;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use trade_control_core::{broker_exit::BrokerTradeExit, intent::Direction, state::EntryAttempt};

pub use request::recover;
pub use storage::persist;

/// A read-only broker proof, independent of persistence and the operator UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillProof {
    pub account: String,
    pub account_id: i64,
    pub audit_id: String,
    pub instrument: String,
    pub order_id: String,
    pub position_id: String,
    pub direction: Direction,
    pub units: f64,
    pub entry_price: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub pip_size: f64,
    pub opened_at: DateTime<Utc>,
    pub requested_stop_loss: f64,
    pub requested_take_profit: f64,
    pub audit_intent: Value,
    pub evidence: Value,
    pub closed_exit: Option<BrokerTradeExit>,
}

/// Authenticated attachment plan. No invented timeframe, break-even or rules.
#[derive(Debug, Clone, Serialize)]
pub struct Adoption {
    proof: FillProof,
    request_id: String,
    intent_id: String,
    attempt: EntryAttempt,
}

impl Adoption {
    pub fn proof(&self) -> &FillProof {
        &self.proof
    }
    pub fn attempt(&self) -> &EntryAttempt {
        &self.attempt
    }
}

#[cfg(test)]
mod postgres_tests;
#[cfg(test)]
mod tests;
