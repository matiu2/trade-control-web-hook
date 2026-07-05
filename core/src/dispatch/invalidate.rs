//! The `Invalidate` dispatch path.

use super::action_result::ActionResult;
use super::shared::{record_control_event_for, resolve_phase1_u32};
use crate::broker::Broker;
use crate::incoming;
use crate::state::StateStore;

/// Dispatch an `Invalidate` intent: set an instrument cooldown and cancel any
/// pending orders for it. Extracted from `run_action` so the cron engine can
/// dispatch a fired invalidation veto through the identical path.
pub async fn run_invalidate<B: Broker, S: StateStore>(
    broker: &B,
    store: &S,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> ActionResult {
    let hours = match resolve_phase1_u32(
        "cooldown_hours",
        verified.intent.cooldown_hours.as_ref(),
        &verified.shell,
        12,
    ) {
        Ok(n) => n,
        Err(outcome) => {
            return ActionResult::Rejected {
                status: 412,
                body: "cooldown_hours script error".to_string(),
                outcome,
            };
        }
    };
    let account = verified.intent.account.as_deref();
    if let Err(err) = store
        .set_cooldown(account, &verified.intent.instrument, hours, now)
        .await
    {
        tracing::error!("KV set_cooldown: {err}");
        return ActionResult::Rejected {
            status: 500,
            body: "state error".to_string(),
            outcome: "rejected: state-error".into(),
        };
    }
    record_control_event_for(
        store,
        account,
        verified.intent.trade_id.as_deref(),
        crate::control_event::ControlKind::Cooldown,
        "",
        &verified.intent.instrument,
        (hours as u64).saturating_mul(3600),
        now,
        None,
    )
    .await;
    let cancelled = broker
        .cancel_pending_for_instrument(&verified.intent.instrument)
        .await;
    tracing::info!(
        "invalidate instrument={} account={} cooldown={}h cancelled={} pending",
        verified.intent.instrument,
        account.unwrap_or("<global>"),
        hours,
        cancelled
    );
    ActionResult::Ok(format!(
        "invalidated: cooldown {hours}h, cancelled {cancelled}"
    ))
}
