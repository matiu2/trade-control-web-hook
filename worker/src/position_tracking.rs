//! Read-only broker observations. Reporting failures never change trading state.
mod storage;
pub(crate) use storage::{enrich_list, for_trade, sync_for_trade};

use crate::{NativeCronEnv, PgStateStore};
use chrono::Utc;
use color_eyre::{Result, eyre::eyre};
use trade_control_core::position_tracking::{
    PositionCheck, PositionLink, observe as observe_broker,
};
use trade_control_cron::{BrokerHandle, CronEnv};

pub(crate) async fn poll(store: &PgStateStore, cron: &NativeCronEnv) {
    if let Err(error) = poll_inner(store, cron).await {
        tracing::error!(%error, "position reporting failed; previous snapshots retained, will retry");
    }
}

async fn poll_inner(store: &PgStateStore, cron: &NativeCronEnv) -> Result<()> {
    let links = storage::candidate_links(store.pool()).await?;
    let accounts: std::collections::BTreeSet<_> =
        links.iter().map(|link| link.account.clone()).collect();
    for account in accounts {
        let mut candidates: Vec<_> = links
            .iter()
            .filter(|link| link.account == account)
            .cloned()
            .collect();
        candidates.dedup();
        let result = match cron.acquire_engine_broker(account.as_deref()).await {
            Ok(broker) => {
                observe(
                    &broker,
                    account.as_deref().unwrap_or_default(),
                    &mut candidates,
                )
                .await
            }
            Err(error) => Err(eyre!("{error}")),
        };
        let now = Utc::now();
        let recorded = match result {
            Ok(observation) => {
                async {
                    storage::record(
                        store.pool(),
                        account.as_deref(),
                        observation.observed_at,
                        &observation.positions,
                        &observation.exits,
                        &observation.warnings,
                    )
                    .await?;
                    crate::position_links::finish_cancelled(store.pool(), &observation.cancelled)
                        .await
                }
                .await
            }
            Err(error) => {
                tracing::warn!(?account, %error, "broker position refresh failed; retaining last observation");
                storage::failure(store.pool(), account.as_deref(), now, &error.to_string()).await
            }
        };
        if let Err(error) = recorded {
            tracing::error!(?account, %error, "position persistence failed; retaining snapshots and continuing other accounts");
            if let Err(error) =
                storage::failure(store.pool(), account.as_deref(), now, &error.to_string()).await
            {
                tracing::error!(?account, %error, "could not persist position refresh failure; observation timestamp remains stale");
            }
        }
    }
    Ok(())
}

async fn observe(
    broker: &BrokerHandle,
    account: &str,
    links: &mut [PositionLink],
) -> Result<PositionCheck> {
    match broker {
        BrokerHandle::Oanda(b) => observe_broker(b, account, links, Utc::now())
            .await
            .map_err(|error| eyre!("{error:?}")),
        BrokerHandle::TradeNation(b) => observe_broker(b, account, links, Utc::now())
            .await
            .map_err(|error| eyre!("{error:?}")),
        BrokerHandle::Ibkr(b) => observe_broker(b, account, links, Utc::now())
            .await
            .map_err(|error| eyre!("{error:?}")),
        BrokerHandle::Mt5(b) => observe_broker(b, account, links, Utc::now())
            .await
            .map_err(|error| eyre!("{error:?}")),
        BrokerHandle::Ctrader(b) => observe_broker(b, account, links, Utc::now())
            .await
            .map_err(|error| eyre!("{error:?}")),
    }
}
