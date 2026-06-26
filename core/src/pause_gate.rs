//! Shared news-blackout (pause/resume) enforcement.
//!
//! A plan folds each calendar event into a `pause`/`resume` pair of
//! `TimeReached` rules. When `pause` fires the trade is blacked out; while it
//! is blacked out an `enter` must not place; the matching `resume` lifts it.
//!
//! Both the live worker and the offline `replay-candles` simulator must make
//! the **same** decision, so the three operations live here once, over the
//! `core::state::StateStore` trait, and both consumers call them:
//!
//! - [`apply_pause`] — a fired `pause` intent sets a blackout (the worker's
//!   `handle_pause`, cron `dispatch_action`).
//! - [`apply_resume`] — a fired `resume` intent clears it (`handle_resume`).
//! - [`entry_blocked`] — an `enter`'s blackout gate (the worker's `run_enter`,
//!   the replay's enter-dispatch path).
//!
//! Keeping the decision in one place is the lesson of every replay-vs-worker
//! drift bug: a trade-rule change must land where both see it. The worker's
//! `handle_pause`/`handle_resume`/`run_enter` wrappers add their own HTTP
//! response + recording shells around these calls, but the *decision* — what
//! state a fire writes and when an entry is blocked — is exactly this module.

use chrono::{DateTime, Utc};

use crate::intent::Intent;
use crate::state::{StateError, StateStore, veto_ttl_seconds};

/// Why an entry was allowed or blocked by the blackout gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryGate {
    /// No active pause for this trade — the entry may proceed.
    Allowed,
    /// At least one pause is active; the entry is blocked. Carries the active
    /// blackout ids (with reason in parens when present) for logging.
    Blocked { blackouts: Vec<String> },
}

impl EntryGate {
    /// Convenience: is the entry blocked?
    pub fn is_blocked(&self) -> bool {
        matches!(self, EntryGate::Blocked { .. })
    }
}

/// Apply a fired `pause` intent: arm a blackout for `(trade_id, blackout_id)`.
///
/// Mirrors the worker's `handle_pause`: the TTL is a safety net derived from
/// `not_after` (the matching `resume` is the authoritative clear), so a dropped
/// resume can't pin the trade forever. Returns `Ok(false)` (a no-op) when the
/// intent lacks the `trade_id` / `blackout_id` a pause requires — the same
/// shape the worker rejects with a 400, but callers in a fire loop just skip.
pub async fn apply_pause<S: StateStore>(
    store: &S,
    intent: &Intent,
    now: DateTime<Utc>,
) -> Result<bool, StateError> {
    let (Some(trade_id), Some(blackout_id)) =
        (intent.trade_id.as_deref(), intent.blackout_id.as_deref())
    else {
        return Ok(false);
    };
    let ttl_seconds = veto_ttl_seconds(0, intent.not_after, now);
    store
        .set_pause(
            trade_id,
            blackout_id,
            intent.reason.as_deref(),
            now,
            ttl_seconds,
        )
        .await?;
    Ok(true)
}

/// Apply a fired `resume` intent: clear the `(trade_id, blackout_id)` pause.
/// Siblings (other blackout ids on the same trade) survive. Returns whether a
/// pause was actually cleared (`false` if none was set, or the intent lacks the
/// required ids).
pub async fn apply_resume<S: StateStore>(store: &S, intent: &Intent) -> Result<bool, StateError> {
    let (Some(trade_id), Some(blackout_id)) =
        (intent.trade_id.as_deref(), intent.blackout_id.as_deref())
    else {
        return Ok(false);
    };
    store.clear_pause(trade_id, blackout_id).await
}

/// The blackout gate an `enter` passes through: if any pause for the entry's
/// `trade_id` is active, the entry is blocked.
///
/// Mirrors the worker's `run_enter` blackout gate. An entry with no `trade_id`
/// (legacy single-shot) can't be looked up and is always [`EntryGate::Allowed`]
/// — the same bypass the worker has.
pub async fn entry_blocked<S: StateStore>(
    store: &S,
    intent: &Intent,
) -> Result<EntryGate, StateError> {
    let Some(trade_id) = intent.trade_id.as_deref() else {
        return Ok(EntryGate::Allowed);
    };
    let pauses = store.list_pauses_for_trade(trade_id).await?;
    if pauses.is_empty() {
        return Ok(EntryGate::Allowed);
    }
    let blackouts = pauses
        .iter()
        .map(|p| match &p.reason {
            Some(r) => format!("{}({r})", p.blackout_id),
            None => p.blackout_id.clone(),
        })
        .collect();
    Ok(EntryGate::Blocked { blackouts })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MemStateStore;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        pollster::block_on(f)
    }

    fn now() -> DateTime<Utc> {
        "2026-05-29T04:00:00Z".parse().unwrap()
    }

    /// A `pause`/`resume` intent in the shape `build-trade` emits and the
    /// engine fires. `reason` is omitted when `None`.
    fn pause_intent(
        action: &str,
        trade_id: &str,
        blackout_id: &str,
        reason: Option<&str>,
    ) -> Intent {
        let reason_line = reason.map(|r| format!("\nreason: {r}")).unwrap_or_default();
        let yaml = format!(
            "v: 1\nid: {action}-{trade_id}-{blackout_id}\nnot_after: 2026-05-30T00:00:00Z\n\
             action: {action}\ninstrument: CAD_CHF\ntrade_id: {trade_id}\n\
             blackout_id: {blackout_id}{reason_line}"
        );
        serde_yaml::from_str(&yaml).expect("parse pause intent")
    }

    /// An `enter` intent; `trade_id` omitted entirely when `None` (the legacy
    /// single-shot bypass case).
    fn enter_intent(trade_id: Option<&str>) -> Intent {
        let tid_line = trade_id
            .map(|t| format!("\ntrade_id: {t}"))
            .unwrap_or_default();
        let yaml = format!(
            "v: 1\nid: enter-x\nnot_after: 2026-05-30T00:00:00Z\n\
             action: enter\ninstrument: CAD_CHF\ndirection: short{tid_line}"
        );
        serde_yaml::from_str(&yaml).expect("parse enter intent")
    }

    #[test]
    fn pause_blocks_then_resume_allows() {
        run(async {
            let store = MemStateStore::default();
            // Replay clock pinned to the historical tick time, so a pause whose
            // TTL ends ~2026-05-30 isn't judged expired against today's wall-clock.
            store.set_clock(now());
            let enter = enter_intent(Some("t041"));

            // No pause yet → allowed.
            assert_eq!(
                entry_blocked(&store, &enter).await.unwrap(),
                EntryGate::Allowed
            );

            // Pause fires → entry blocked, blackout id surfaced with reason.
            let pause = pause_intent("pause", "t041", "cad-gdp", Some("CAD GDP"));
            assert!(apply_pause(&store, &pause, now()).await.unwrap());
            let gate = entry_blocked(&store, &enter).await.unwrap();
            assert_eq!(
                gate,
                EntryGate::Blocked {
                    blackouts: vec!["cad-gdp(CAD GDP)".into()]
                }
            );
            assert!(gate.is_blocked());

            // Resume fires → pause cleared → entry allowed again.
            let resume = pause_intent("resume", "t041", "cad-gdp", None);
            assert!(apply_resume(&store, &resume).await.unwrap());
            assert_eq!(
                entry_blocked(&store, &enter).await.unwrap(),
                EntryGate::Allowed
            );
        });
    }

    #[test]
    fn sibling_blackout_keeps_trade_paused_after_one_resume() {
        run(async {
            let store = MemStateStore::default();
            store.set_clock(now());
            let enter = enter_intent(Some("t041"));
            apply_pause(
                &store,
                &pause_intent("pause", "t041", "generic", None),
                now(),
            )
            .await
            .unwrap();
            apply_pause(
                &store,
                &pause_intent("pause", "t041", "cad-gdp", None),
                now(),
            )
            .await
            .unwrap();

            // Resume only the generic one — the cad-gdp blackout still blocks.
            apply_resume(&store, &pause_intent("resume", "t041", "generic", None))
                .await
                .unwrap();
            assert!(entry_blocked(&store, &enter).await.unwrap().is_blocked());
        });
    }

    #[test]
    fn entry_without_trade_id_bypasses_the_gate() {
        run(async {
            let store = MemStateStore::default();
            store.set_clock(now());
            // A pause exists for some trade, but this enter has no trade_id.
            apply_pause(
                &store,
                &pause_intent("pause", "t041", "cad-gdp", None),
                now(),
            )
            .await
            .unwrap();
            assert_eq!(
                entry_blocked(&store, &enter_intent(None)).await.unwrap(),
                EntryGate::Allowed
            );
        });
    }

    #[test]
    fn pause_without_ids_is_a_noop() {
        run(async {
            let store = MemStateStore::default();
            // An enter-shaped intent (no blackout_id) handed to apply_pause is a no-op.
            let bad = enter_intent(Some("x"));
            assert!(!apply_pause(&store, &bad, now()).await.unwrap());
        });
    }
}
