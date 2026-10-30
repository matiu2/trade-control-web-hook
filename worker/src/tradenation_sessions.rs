//! Shared native TradeNation sessions and background health maintenance.
//!
//! One authenticated client per account name and demo/live kind, across all
//! native runtimes. Health checks run every 30–34s for recently used accounts,
//! every 300–304s otherwise. Only confirmed expiry triggers reauthentication;
//! an inconclusive probe preserves the session and retries with 5–120s backoff
//! plus a 0–4s account stagger. The pool does not persist cookies or credentials.
//! Acquisition also validates sessions last checked at least 30s ago, so the
//! first entry after idle does not wait for the slower background cadence.

mod backend;
mod maintenance;
mod pool;
mod schedule;

use std::sync::{Arc, OnceLock};

use broker_tradenation::TradeNationBroker;
use trade_control_core::account::AccountMetadata;

use crate::BrokerError;
use backend::TradeNation;
use pool::{Key, Pool};

pub use maintenance::start_tradenation_session_maintenance;

fn shared() -> &'static Arc<Pool<TradeNation>> {
    static POOL: OnceLock<Arc<Pool<TradeNation>>> = OnceLock::new();
    POOL.get_or_init(|| Arc::new(Pool::new(TradeNation)))
}

pub(crate) async fn broker(meta: &AccountMetadata) -> Result<Arc<TradeNationBroker>, BrokerError> {
    shared()
        .get(&Key::from(meta))
        .await
        .map_err(BrokerError::TradeNationLogin)
}

impl From<&AccountMetadata> for Key {
    fn from(meta: &AccountMetadata) -> Self {
        Self {
            name: meta.name.clone(),
            live: meta.kind.is_live(),
        }
    }
}

#[cfg(test)]
mod tests;
