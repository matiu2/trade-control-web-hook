//! Process-owned Tokio task: discover accounts, prewarm, and maintain shared sessions.

use std::{future::Future, sync::Arc, time::Duration};

use tokio::{
    task::{JoinHandle, JoinSet},
    time::{Instant, MissedTickBehavior, interval, timeout},
};
use trade_control_core::{account::MetadataStore, intent::BrokerKind};

use crate::PgMetadataStore;

use super::{
    Key,
    pool::{Backend, Pool},
    shared,
};

/// Start once on the main runtime. Aborting this handle drops the JoinSet and
/// cancels all probes/logins; it does not detach per-account tasks at shutdown.
pub fn start_tradenation_session_maintenance(accounts: PgMetadataStore) -> JoinHandle<()> {
    tokio::spawn(run(shared().clone(), move || {
        let accounts = accounts.clone();
        async move { discover(&accounts).await }
    }))
}

pub(super) async fn run<B, F, Fut>(pool: Arc<Pool<B>>, discovery: F)
where
    B: Backend,
    F: Fn() -> Fut,
    Fut: Future<Output = Option<Vec<Key>>> + Send + 'static,
{
    let mut ticker = interval(Duration::from_secs(5));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut next_discovery = Instant::now();
    let mut jobs = JoinSet::new();
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if Instant::now() >= next_discovery {
                    let started = Instant::now();
                    let accounts = discovery();
                    let pool = pool.clone();
                    jobs.spawn(async move {
                        if let Some(keys) = accounts.await {
                            pool.reconcile(&keys, started).await;
                        }
                    });
                    next_discovery = Instant::now() + Duration::from_secs(60);
                }
                pool.maintain_due(&mut jobs).await;
            }
            Some(result) = jobs.join_next(), if !jobs.is_empty() => {
                if let Err(error) = result {
                    tracing::error!(%error, "TradeNation session maintenance task failed");
                }
            }
        }
    }
}

async fn discover(accounts: &PgMetadataStore) -> Option<Vec<Key>> {
    match timeout(Duration::from_secs(10), accounts.list()).await {
        Ok(Ok(accounts)) => {
            return Some(
                accounts
                    .iter()
                    .filter(|meta| meta.broker == BrokerKind::TradeNation)
                    .map(Key::from)
                    .collect(),
            );
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "TradeNation session account discovery failed; retrying next minute")
        }
        Err(error) => {
            tracing::warn!(%error, "TradeNation session account discovery timed out; retrying next minute")
        }
    }
    None
}
