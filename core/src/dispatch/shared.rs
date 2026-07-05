//! Helpers shared by more than one dispatch submodule.

use crate::intent::Shell;
use crate::rules::{self, RuleError};
use crate::state::StateStore;
use crate::tunable::Tunable;

/// Resolve a [`Tunable<u32>`] against Phase 1 scope only (shell
/// anchors). Used by the `Invalidate`, `Prep`, and `Veto` action
/// paths — none of which builds a `Resolved`, so derived geometry
/// bindings aren't available. `default` is the fallback when the
/// field is absent. On script error returns a telemetry string the
/// caller wraps into an `ActionResult::Rejected`.
pub fn resolve_phase1_u32(
    field: &'static str,
    tunable: Option<&Tunable<u32>>,
    shell: &Shell,
    default: u32,
) -> Result<u32, String> {
    let Some(t) = tunable else { return Ok(default) };
    let engine = rules::build_engine();
    let mut scope = rules::RhaiScope::new();
    rules::bind_shell_anchors(&mut scope, shell);
    rules::resolve_tunable::<u32>(&engine, &mut scope, t).map_err(|err| {
        let kind = match &err {
            RuleError::Parse(_) => "parse",
            RuleError::Eval(_) => "eval",
            RuleError::WrongType { .. } => "wrong-type",
        };
        format!("rejected: {field}-script-{kind}")
    })
}

/// Append a [`ControlEvent`] audit row alongside a TTL'd control set.
///
/// Best-effort and **non-blocking**: a failure is logged and swallowed — the
/// live control row was already set, and the audit trail must never gate it.
/// Skipped when there's no `trade_id` to scope it to (the trail is per-trade;
/// a `cooldown`/blackout set without a trade_id can't be journaled per trade).
/// `request_id` links the event back to its R2 `req/` bundle when known.
#[allow(clippy::too_many_arguments)]
pub async fn record_control_event_for<S: StateStore>(
    store: &S,
    account: Option<&str>,
    trade_id: Option<&str>,
    kind: crate::control_event::ControlKind,
    name: &str,
    instrument: &str,
    ttl_seconds: u64,
    now: chrono::DateTime<chrono::Utc>,
    request_id: Option<String>,
) {
    let Some(trade_id) = trade_id else {
        return;
    };
    let event = crate::control_event::ControlEvent::new(
        kind,
        name,
        instrument,
        now,
        ttl_seconds,
        request_id,
    );
    if let Err(err) = store.record_control_event(account, trade_id, &event).await {
        tracing::error!("KV record_control_event ({}/{name}): {err}", kind.tag());
    }
}
