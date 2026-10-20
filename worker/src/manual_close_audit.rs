//! Read-only history checks after accepting the operator's closure assertion.
//! Failures update the audit record, never revive management or send an order.

mod history;
mod storage;

use std::time::Duration;

use trade_control_cron::CronEnv;

use crate::{NativeCronEnv, PgStateStore};

pub(crate) async fn poll(store: &PgStateStore, cron: &NativeCronEnv) {
    match tokio::time::timeout(Duration::from_secs(45), poll_inner(store, cron)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(%error, "manual-close history audit failed"),
        Err(_) => tracing::warn!(
            "manual-close history audit time budget reached; claimed jobs remain due after cooldown"
        ),
    }
}

async fn poll_inner(store: &PgStateStore, cron: &NativeCronEnv) -> color_eyre::Result<()> {
    let jobs = storage::claim(store.pool()).await?;
    let Some(first) = jobs.first() else {
        return Ok(());
    };
    // One account per pass, and at most one login per account per 30 minutes.
    let needs_history = jobs
        .iter()
        .any(|job| history::attempts(&job.body).is_ok_and(|attempts| !attempts.is_empty()));
    let broker = if needs_history {
        tokio::time::timeout(
            Duration::from_secs(10),
            cron.acquire_engine_broker(first.account.as_deref()),
        )
        .await
        .map_err(|_| "broker history connection timed out".to_owned())
        .and_then(std::convert::identity)
    } else {
        Err("no broker history lookup required".into())
    };
    for job in jobs {
        let result = match history::attempts(&job.body) {
            Ok(attempts) if attempts.is_empty() => history::Audit::Unavailable(
                "no recorded broker order IDs to match against closed-trade history".into(),
            ),
            Err(error) => history::Audit::Unavailable(format!(
                "cannot read detached broker attempts: {error}"
            )),
            Ok(_) => match &broker {
                Ok(broker) => match tokio::time::timeout(
                    Duration::from_secs(10),
                    history::check(broker, &job.body),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => history::Audit::Pending(
                        "broker closed-trade history lookup timed out".into(),
                    ),
                },
                Err(error) => history::Audit::Pending(error.clone()),
            },
        };
        storage::finish(store.pool(), &job, result).await?;
    }
    Ok(())
}
