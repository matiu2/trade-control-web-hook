//! Signed operator plan transitions, applied atomically without broker calls.

pub(crate) mod guard;
mod mapping;
mod storage;
#[cfg(test)]
mod storage_tests;

use chrono::{DateTime, Utc};
use trade_control_core::{
    account::MetadataStore,
    dispatch::{ControlResult, record_seen},
    incoming::Verified,
    intent::Action,
};

use crate::http::AppState;

pub(crate) async fn handle(
    state: &AppState,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return ControlResult::error("plan administration requires trade_id", 400);
    };
    let Some(options) = verified.intent.plan_admin.as_ref() else {
        return ControlResult::error(
            "plan administration requires signed plan_admin choices",
            400,
        );
    };
    if !options.detach_existing {
        return ControlResult::error(
            "explicit detach_existing is required; old broker exposure will not be closed or migrated",
            422,
        );
    }
    let target = if matches!(
        verified.intent.action,
        Action::PlanPaper | Action::PlanSwapAccount
    ) {
        let Some(name) = options
            .target_account
            .as_deref()
            .filter(|name| !name.is_empty())
        else {
            return ControlResult::error(
                "choose an explicit target account (paper uses it only for market data)",
                422,
            );
        };
        match state.accounts.get(name).await {
            Ok(target) => Some(target),
            Err(error) => return ControlResult::error(format!("target account: {error}"), 422),
        }
    } else {
        None
    };
    match storage::transition(
        state.store.pool(),
        trade_id,
        verified.intent.action,
        target.as_ref(),
        now,
    )
    .await
    {
        Ok(message) => {
            record_seen(&state.store, verified, now, &message).await;
            tracing::info!(trade_id, %message, "plan administration completed");
            ControlResult::ok(message)
        }
        Err(error) => {
            tracing::error!(trade_id, %error, "plan administration failed");
            let status = if error.downcast_ref::<sqlx::Error>().is_some() {
                500
            } else {
                409
            };
            ControlResult::error(error.to_string(), status)
        }
    }
}
