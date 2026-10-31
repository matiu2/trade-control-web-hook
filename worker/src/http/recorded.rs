//! Native reporting reads include durable orders that have no engine plan.
use trade_control_core::dispatch::ControlResult;

pub(crate) async fn list(
    pool: &sqlx::PgPool,
    result: ControlResult,
    include_all: bool,
) -> ControlResult {
    if !result.is_success() {
        return result;
    }
    let enriched = async {
        let body = if include_all {
            crate::recorded_trades::enrich_list(pool, &result.body).await?
        } else {
            result.body
        };
        crate::position_tracking::enrich_list(pool, &body).await
    }
    .await;
    match enriched {
        Ok(body) => ControlResult::ok(body),
        Err(error) => {
            tracing::error!(%error, "journal list reporting unavailable");
            ControlResult::error("journal list reporting unavailable", 500)
        }
    }
}

pub(crate) async fn show(
    pool: &sqlx::PgPool,
    result: ControlResult,
    target: Option<&str>,
) -> ControlResult {
    if result.status != 404 {
        return result;
    }
    let Some(target) = target else {
        return result;
    };
    match crate::recorded_trades::show(pool, target)
        .await
        .and_then(|entry| {
            entry
                .map(|entry| serde_yaml::to_string(&vec![entry]))
                .transpose()
                .map_err(Into::into)
        }) {
        Ok(Some(body)) => ControlResult::ok(body),
        Ok(None) => result,
        Err(error) => {
            tracing::error!(%error, %target, "recorded order detail unavailable");
            ControlResult::error("recorded order detail unavailable", 500)
        }
    }
}
