//! Native authentication and read-only session validation. Never replay broker writes.

use broker_tradenation::TradeNationBroker;
use trade_control_core::account::AccountKind;

use super::pool::{Backend, Key, Probe};

pub(super) struct TradeNation;

impl Backend for TradeNation {
    type Session = TradeNationBroker;

    async fn login(&self, key: &Key) -> Result<TradeNationBroker, String> {
        let kind = if key.live {
            AccountKind::Live
        } else {
            AccountKind::Demo
        };
        let session = crate::broker_factory::tradenation::session(&key.name, kind)
            .await
            .map_err(|error| error.to_string())?;
        let json = serde_json::to_string(&session)
            .map_err(|error| format!("serialize session: {error}"))?;
        broker_tradenation::login(&json)
            .await
            .ok_or_else(|| "TradeNation rejected the newly authenticated session".into())
    }

    async fn probe(&self, broker: &TradeNationBroker) -> Probe {
        match tradenation_api::validate_session(broker.client(), broker.session()).await {
            Ok(()) => Probe::Healthy,
            Err(error) if error.is_session_expired() => Probe::Expired,
            Err(error) => {
                tracing::debug!(%error, "TradeNation session health check failed without confirming expiry");
                Probe::Inconclusive
            }
        }
    }
}
