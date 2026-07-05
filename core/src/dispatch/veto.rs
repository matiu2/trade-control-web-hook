//! The broker-side `Veto` dispatch path (`cancel-pending` / `close-positions`).

use super::action_result::ActionResult;
use super::shared::{record_control_event_for, resolve_phase1_u32};
use crate::broker::Broker;
use crate::incoming;
use crate::intent::VetoLevel;
use crate::state::{StateStore, clear_named_vetos, veto_ttl_seconds};

/// Format the seen-index outcome string for a veto. Used by both the
/// flag-only path (the worker's `handle_veto`) and the broker-side path
/// ([`run_veto_with_broker`]). `cancelled` is the count of pending orders
/// the broker cancelled (None for the flag-only path); `closed_tag` is
/// `"closed=ok"` / `"closed=failed"` when a close was attempted (or
/// None otherwise).
pub fn format_veto_set_outcome(
    name: &str,
    ttl_hours: u32,
    level_tag: &str,
    cleared: &[String],
    cancelled: Option<usize>,
    closed_tag: Option<&str>,
) -> String {
    let mut out = format!("veto-set: {name} ttl={ttl_hours}h level={level_tag}");
    if let Some(c) = cancelled {
        out.push_str(&format!(" cancelled={c}"));
    }
    if let Some(t) = closed_tag {
        out.push(' ');
        out.push_str(t);
    }
    if !cleared.is_empty() {
        out.push_str(&format!(" cleared=[{}]", cleared.join(",")));
    }
    out
}

/// Handle the `veto` action at level `cancel-pending` or
/// `close-positions`: set the KV flag, then execute the broker-side
/// effects appropriate to the level. Re-fires repeat the side effects
/// (alerts can drop; reapplying is cheap and defensive).
pub async fn run_veto_with_broker<B: Broker, S: StateStore>(
    broker: &B,
    store: &S,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> ActionResult {
    let Some(name) = verified.intent.name.as_deref() else {
        return ActionResult::Rejected {
            status: 400,
            body: "veto requires `name`".to_string(),
            outcome: "rejected: missing-name".into(),
        };
    };
    // `Intent::validate` guarantees `trade_id` on `veto`; guard here is
    // defence-in-depth (the veto key is scoped per-setup).
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return ActionResult::Rejected {
            status: 400,
            body: "veto requires trade_id".to_string(),
            outcome: "rejected: missing-trade-id".into(),
        };
    };
    let ttl_hours = match resolve_phase1_u32(
        "ttl_hours",
        Some(&verified.intent.ttl_hours),
        &verified.shell,
        0,
    ) {
        Ok(n) => n,
        Err(outcome) => {
            return ActionResult::Rejected {
                status: 412,
                body: "ttl_hours script error".to_string(),
                outcome,
            };
        }
    };
    let level = verified.intent.level.unwrap_or_default();
    // See `veto_ttl_seconds` — the veto must outlive the setup it
    // kills, not just survive a fixed cooldown from "now".
    let ttl_seconds = veto_ttl_seconds(ttl_hours, verified.intent.not_after, now);
    let instrument = &verified.intent.instrument;
    let account = verified.intent.account.as_deref();
    let cleared = match clear_named_vetos(
        store,
        account,
        trade_id,
        instrument,
        &verified.intent.clears,
    )
    .await
    {
        Ok(c) => c,
        Err(err) => {
            tracing::error!("KV clear_named_vetos (in clears): {err}");
            Vec::new()
        }
    };
    if let Err(err) = store
        .set_veto(account, trade_id, instrument, name, ttl_seconds)
        .await
    {
        tracing::error!("KV set_veto: {err}");
        return ActionResult::Rejected {
            status: 500,
            body: "state error".to_string(),
            outcome: "rejected: state-error".into(),
        };
    }
    record_control_event_for(
        store,
        account,
        Some(trade_id),
        crate::control_event::ControlKind::Veto,
        name,
        instrument,
        ttl_seconds,
        now,
        None,
    )
    .await;

    let cancelled = broker.cancel_pending_for_instrument(instrument).await;
    let closed_ok = match level {
        VetoLevel::ClosePositions => broker.close_positions(instrument).await,
        // No close requested at this level.
        VetoLevel::CancelPending | VetoLevel::StopNextEntry => true,
    };

    tracing::info!(
        "veto set: instrument={} account={} name={} ttl={}h level={:?} cancelled={} closed_ok={} cleared={:?}",
        instrument,
        account.unwrap_or("<global>"),
        name,
        ttl_hours,
        level,
        cancelled,
        closed_ok,
        cleared
    );
    let closed_tag = match level {
        VetoLevel::ClosePositions => Some(if closed_ok {
            "closed=ok"
        } else {
            "closed=failed"
        }),
        _ => None,
    };
    let level_tag = match level {
        VetoLevel::StopNextEntry => "stop-next-entry",
        VetoLevel::CancelPending => "cancel-pending",
        VetoLevel::ClosePositions => "close-positions",
    };
    let outcome = format_veto_set_outcome(
        name,
        ttl_hours,
        level_tag,
        &cleared,
        Some(cancelled),
        closed_tag,
    );
    if matches!(level, VetoLevel::ClosePositions) && !closed_ok {
        ActionResult::Failed(outcome)
    } else {
        ActionResult::Ok(outcome)
    }
}
