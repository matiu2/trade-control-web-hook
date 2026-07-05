//! The worker-free control-action handlers.
//!
//! These 15 handlers (`handle_status` / `handle_prep` / `handle_veto` /
//! `handle_register` / `handle_plan_*` / …) set or read KV state and never
//! touch the broker, an `Env`, R2, or the worker recording buffer. They
//! historically returned a `worker::Response` directly (which panics off-wasm
//! and pinned them to Cloudflare); they now return the worker-free
//! [`ControlResult`] carrier, so the wasm worker and any native runtime can map
//! the same `{ status, body }` to their own HTTP edge — one handler, two edges,
//! no drift.
//!
//! They log via plain `tracing::{info,error}!` (the worker's `ConsoleSubscriber`
//! tees those into its R2 recording buffer, exactly as the old `rlog!` /
//! `rlog_err!` macros did).
//!
//! The two `Env`-using control handlers (`handle_plan_purge` /
//! `handle_purge_older_than`) stay in the worker — they do R2 work — and return
//! [`ControlResult`] from there.

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::control_result::ControlResult;
use super::shared::{record_control_event_for, resolve_phase1_u32};
use super::veto::format_veto_set_outcome;
use crate::control_event::ControlKind;
use crate::incoming::{self, Verified};
use crate::state::{
    ArchivedPlan, StateError, StateStore, StoredPlan, clear_named_preps, clear_named_vetos,
    veto_ttl_seconds,
};

/// Response body for the `unlock` action. Serialised as YAML.
#[derive(Serialize)]
struct UnlockResponse {
    unlocked: String,
    was_cooled_down: bool,
}

/// Handle the `status` action: dump cooldown + recent-seen indexes as YAML.
pub async fn handle_status<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let snap = match store.snapshot().await {
        Ok(s) => s,
        Err(err) => {
            tracing::error!("KV snapshot: {err}");
            return ControlResult::error("state error", 500);
        }
    };
    let body = match serde_yaml::to_string(&snap) {
        Ok(s) => s,
        Err(err) => {
            tracing::error!("snapshot serialise: {err}");
            return ControlResult::error("internal error", 500);
        }
    };
    record_seen(store, verified, now, "status").await;
    ControlResult::ok(body)
}

/// Best-effort wrapper around `mark_seen`. Used by the dedicated control
/// handlers (status / unlock / prep / veto / clear-*) so each one ends
/// with one line instead of an `if let Err` repeated everywhere.
pub async fn record_seen<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
    outcome: &str,
) {
    let ttl = incoming::replay_ttl_seconds(verified.intent.not_after, now);
    if let Err(err) = store
        .mark_seen(
            &verified.intent.id,
            verified.intent.action,
            now,
            outcome,
            ttl,
            verified.intent.trade_id.as_deref(),
        )
        .await
    {
        tracing::error!("KV mark_seen ({outcome}): {err}");
    }
}

/// Handle the `unlock` action: clear the cooldown for `verified.intent.instrument`.
pub async fn handle_unlock<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let instrument = &verified.intent.instrument;
    let account = verified.intent.account.as_deref();
    let was = match store.clear_cooldown(account, instrument).await {
        Ok(b) => b,
        Err(err) => {
            tracing::error!("KV clear_cooldown: {err}");
            return ControlResult::error("state error", 500);
        }
    };
    tracing::info!(
        "unlock instrument={instrument} account={} was_cooled_down={was}",
        account.unwrap_or("<global>")
    );
    let body = match serde_yaml::to_string(&UnlockResponse {
        unlocked: instrument.clone(),
        was_cooled_down: was,
    }) {
        Ok(s) => s,
        Err(err) => {
            tracing::error!("unlock serialise: {err}");
            return ControlResult::error("internal error", 500);
        }
    };
    let outcome = if was { "unlocked" } else { "unlocked: noop" };
    record_seen(store, verified, now, outcome).await;
    ControlResult::ok(body)
}

/// Handle the `prep` action: record a named step for an instrument with a TTL.
pub async fn handle_prep<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let Some(step) = verified.intent.step.as_deref() else {
        return ControlResult::error("prep requires `step`", 400);
    };
    let ttl_hours = match resolve_phase1_u32(
        "ttl_hours",
        Some(&verified.intent.ttl_hours),
        &verified.shell,
        0,
    ) {
        Ok(n) => n,
        Err(_outcome) => {
            return ControlResult::error("ttl_hours script error", 412);
        }
    };
    let ttl_seconds = (ttl_hours as u64).saturating_mul(3600);
    let account = verified.intent.account.as_deref();
    // Prep-expiry gate (step 2 of the prep-expire flow): if a
    // `<step>-expiry` line has fired for this step, the window for
    // landing it has lapsed — reject the prep so the entry's
    // `requires_preps` can never be satisfied. Rejected (not Ok): does
    // NOT poison the seen-id (a re-fire just re-logs and re-rejects),
    // consistent with the replay-scope rule in CLAUDE.md. An operator
    // reconstructing a trade greps `prep rejected — expired`.
    match store
        .is_prep_blocked(account, &verified.intent.instrument, step)
        .await
    {
        Ok(true) => {
            tracing::info!(
                "prep rejected — expired: instrument={} account={} step={} trade_id={} \
                 (a {step}-expiry line already fired)",
                verified.intent.instrument,
                account.unwrap_or("<global>"),
                step,
                verified.intent.trade_id.as_deref().unwrap_or("<none>"),
            );
            return ControlResult::error(format!("prep-expired: {step}"), 409);
        }
        Ok(false) => {}
        Err(err) => {
            tracing::error!("KV is_prep_blocked: {err}");
            return ControlResult::error("state error", 500);
        }
    }
    // Clear any preps listed in `clears` first so stale downstream
    // preps (e.g. an old `retest`) can't survive a fresh upstream prep
    // (`break-and-close`). Logged per-name for traceability; failures
    // are best-effort logs rather than rejections so a transient KV
    // hiccup on a clear doesn't block the new prep.
    let cleared = match clear_named_preps(
        store,
        account,
        &verified.intent.instrument,
        &verified.intent.clears,
    )
    .await
    {
        Ok(c) => c,
        Err(err) => {
            tracing::error!("KV clear_named_preps (in clears): {err}");
            Vec::new()
        }
    };
    if let Err(err) = store
        .set_prep(
            account,
            &verified.intent.instrument,
            step,
            now,
            ttl_seconds,
            &verified.intent.id,
        )
        .await
    {
        tracing::error!("KV set_prep: {err}");
        return ControlResult::error("state error", 500);
    }
    record_control_event_for(
        store,
        account,
        verified.intent.trade_id.as_deref(),
        ControlKind::Prep,
        step,
        &verified.intent.instrument,
        ttl_seconds,
        now,
        None,
    )
    .await;
    tracing::info!(
        "prep set: instrument={} account={} step={} ttl={}h cleared={:?}",
        verified.intent.instrument,
        account.unwrap_or("<global>"),
        step,
        ttl_hours,
        cleared
    );
    let outcome = format_prep_set_outcome(step, ttl_hours, &cleared);
    record_seen(store, verified, now, &outcome).await;
    ControlResult::ok("ok")
}

fn format_prep_set_outcome(step: &str, ttl_hours: u32, cleared: &[String]) -> String {
    if cleared.is_empty() {
        format!("prep-set: {step} ttl={ttl_hours}h")
    } else {
        format!(
            "prep-set: {step} ttl={ttl_hours}h cleared=[{}]",
            cleared.join(",")
        )
    }
}

/// Handle the `prep-expire` action: block all *future* `prep` fires for
/// a named step on an instrument, with a TTL. Fired by a `<prep>-expiry`
/// chart line when the window for landing that prep has lapsed (e.g. an
/// H&S break-and-close that never came within the allowed bar count).
///
/// State-only — no broker call. A prep that *already* fired before this
/// block is untouched: the block only stops new ones, so a trade that
/// already legitimately entered is not disturbed. After the block,
/// `handle_prep` rejects the step and the enter gate's `requires_preps`
/// for that step can never be satisfied — see the timeline note in the
/// repo `CLAUDE.md`.
///
/// Marks-seen on completion (idempotent control action, like `prep`):
/// replaying the same `prep-expire` body just re-applies the same block.
pub async fn handle_prep_expire<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let Some(step) = verified.intent.step.as_deref() else {
        return ControlResult::error("prep-expire requires `step`", 400);
    };
    let ttl_hours = match resolve_phase1_u32(
        "ttl_hours",
        Some(&verified.intent.ttl_hours),
        &verified.shell,
        0,
    ) {
        Ok(n) => n,
        Err(_outcome) => {
            return ControlResult::error("ttl_hours script error", 412);
        }
    };
    let ttl_seconds = (ttl_hours as u64).saturating_mul(3600);
    let account = verified.intent.account.as_deref();
    if let Err(err) = store
        .block_prep(account, &verified.intent.instrument, step, now, ttl_seconds)
        .await
    {
        tracing::error!("KV block_prep: {err}");
        return ControlResult::error("state error", 500);
    }
    // Timeline log (step 1 of the prep-expire flow): an operator
    // reconstructing a trade later greps `prep-expire stored` to see
    // when the cutoff fired.
    tracing::info!(
        "prep-expire stored: instrument={} account={} step={} ttl={}h trade_id={}",
        verified.intent.instrument,
        account.unwrap_or("<global>"),
        step,
        ttl_hours,
        verified.intent.trade_id.as_deref().unwrap_or("<none>"),
    );
    let outcome = format!("prep-expire: {step} blocked ttl={ttl_hours}h");
    record_seen(store, verified, now, &outcome).await;
    ControlResult::ok("ok")
}

/// Handle the `veto` action at level `stop-next-entry`: record a named
/// veto for an instrument with a TTL. No broker call.
pub async fn handle_veto<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let Some(name) = verified.intent.name.as_deref() else {
        return ControlResult::error("veto requires `name`", 400);
    };
    // `Intent::validate` guarantees `trade_id` on `veto`; guard here is
    // defence-in-depth. The veto key is scoped per-setup so it can't
    // bleed into a different setup on the same instrument.
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return ControlResult::error("veto requires trade_id", 400);
    };
    let ttl_hours = match resolve_phase1_u32(
        "ttl_hours",
        Some(&verified.intent.ttl_hours),
        &verified.shell,
        0,
    ) {
        Ok(n) => n,
        Err(_outcome) => return ControlResult::error("ttl_hours script error", 412),
    };
    // The veto must outlive the setup it invalidates: if price ran
    // too-high mid-window the original `enter` is dead for the rest
    // of its `not_after`, not just the next `ttl_hours`. See
    // `veto_ttl_seconds` for the full motivating example.
    let ttl_seconds = veto_ttl_seconds(ttl_hours, verified.intent.not_after, now);
    // Clear any vetos listed in `clears` first — symmetry with prep
    // ordering, even though vetos don't carry timestamps. Scoped to
    // this intent's account + trade_id, same as the `set_veto` that
    // follows.
    let account = verified.intent.account.as_deref();
    let cleared = match clear_named_vetos(
        store,
        account,
        trade_id,
        &verified.intent.instrument,
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
        .set_veto(
            account,
            trade_id,
            &verified.intent.instrument,
            name,
            ttl_seconds,
        )
        .await
    {
        tracing::error!("KV set_veto: {err}");
        return ControlResult::error("state error", 500);
    }
    record_control_event_for(
        store,
        account,
        Some(trade_id),
        ControlKind::Veto,
        name,
        &verified.intent.instrument,
        ttl_seconds,
        now,
        None,
    )
    .await;
    tracing::info!(
        "veto set: instrument={} account={} name={} ttl={}h cleared={:?}",
        verified.intent.instrument,
        account.unwrap_or("<global>"),
        name,
        ttl_hours,
        cleared
    );
    let outcome = format_veto_set_outcome(name, ttl_hours, "stop-next-entry", &cleared, None, None);
    record_seen(store, verified, now, &outcome).await;
    ControlResult::ok("ok")
}

/// Handle the `clear-prep` action: drop a single prep flag.
pub async fn handle_clear_prep<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let Some(step) = verified.intent.step.as_deref() else {
        return ControlResult::error("clear-prep requires `step`", 400);
    };
    let account = verified.intent.account.as_deref();
    let cleared_setter = match store
        .clear_prep(account, &verified.intent.instrument, step)
        .await
    {
        Ok(s) => s,
        Err(err) => {
            tracing::error!("KV clear_prep: {err}");
            return ControlResult::error("state error", 500);
        }
    };
    // If the cleared prep recorded its setter's message-id, drop that
    // `seen:<id>` record too so the operator can re-send the original
    // prep message without hitting the replay-protection 409.
    if let Some(setter_id) = cleared_setter.as_deref()
        && !setter_id.is_empty()
        && let Err(err) = store.forget_seen(setter_id).await
    {
        // Best-effort — the prep is gone, the operator can manually
        // delete the seen key via wrangler if needed.
        tracing::error!("KV forget_seen({setter_id}): {err}");
    }
    let was = cleared_setter.is_some();
    tracing::info!(
        "clear-prep instrument={} account={} step={} was_set={}",
        verified.intent.instrument,
        account.unwrap_or("<global>"),
        step,
        was
    );
    let outcome = if was {
        format!("prep-cleared: {step}")
    } else {
        format!("prep-cleared: {step} (noop)")
    };
    record_seen(store, verified, now, &outcome).await;
    ControlResult::ok("ok")
}

/// Handle the `clear-veto` action: drop a single veto flag.
pub async fn handle_clear_veto<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let Some(name) = verified.intent.name.as_deref() else {
        return ControlResult::error("clear-veto requires `name`", 400);
    };
    // `Intent::validate` guarantees `trade_id` on `clear-veto`; the veto
    // key is scoped per-setup so a clear only drops this setup's veto.
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return ControlResult::error("clear-veto requires trade_id", 400);
    };
    let account = verified.intent.account.as_deref();
    let was = match store
        .clear_veto(account, trade_id, &verified.intent.instrument, name)
        .await
    {
        Ok(b) => b,
        Err(err) => {
            tracing::error!("KV clear_veto: {err}");
            return ControlResult::error("state error", 500);
        }
    };
    tracing::info!(
        "clear-veto instrument={} account={} name={} was_set={}",
        verified.intent.instrument,
        account.unwrap_or("<global>"),
        name,
        was
    );
    let outcome = if was {
        format!("veto-cleared: {name}")
    } else {
        format!("veto-cleared: {name} (noop)")
    };
    record_seen(store, verified, now, &outcome).await;
    ControlResult::ok("ok")
}

/// Handle the `pause` action: arm a blackout for `(trade_id, blackout_id)`.
/// No broker work. The KV entry's TTL is keyed off `not_after` (plus a
/// grace tail) so an orphaned pause from a dropped `resume` eventually
/// ages out instead of pinning the trade forever. The matching `resume`
/// is the authoritative clear.
pub async fn handle_pause<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return ControlResult::error("pause requires `trade_id`", 400);
    };
    let Some(blackout_id) = verified.intent.blackout_id.as_deref() else {
        return ControlResult::error("pause requires `blackout_id`", 400);
    };
    // Safety TTL: the resume should clear this long before, but a
    // dropped alert shouldn't pin the trade forever. Reuse the veto
    // TTL math — `ttl_hours` may be absent (defaults to the bare
    // floor below), but `not_after + grace` is always honoured so
    // the pause survives at least the alert window.
    let ttl_seconds = veto_ttl_seconds(0, verified.intent.not_after, now);
    let reason = verified.intent.reason.as_deref();
    if let Err(err) = store
        .set_pause(trade_id, blackout_id, reason, now, ttl_seconds)
        .await
    {
        tracing::error!("KV set_pause: {err}");
        return ControlResult::error("state error", 500);
    }
    record_control_event_for(
        store,
        verified.intent.account.as_deref(),
        Some(trade_id),
        ControlKind::Pause,
        blackout_id,
        &verified.intent.instrument,
        ttl_seconds,
        now,
        None,
    )
    .await;
    tracing::info!(
        "pause set: trade_id={trade_id} blackout_id={blackout_id} reason={:?}",
        reason
    );
    let outcome = match reason {
        Some(r) => format!("pause-set: {blackout_id} ({r})"),
        None => format!("pause-set: {blackout_id}"),
    };
    record_seen(store, verified, now, &outcome).await;
    ControlResult::ok("ok")
}

/// Handle the `resume` action: clear a single `(trade_id, blackout_id)`
/// pause. Sibling blackouts on the same trade survive.
pub async fn handle_resume<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return ControlResult::error("resume requires `trade_id`", 400);
    };
    let Some(blackout_id) = verified.intent.blackout_id.as_deref() else {
        return ControlResult::error("resume requires `blackout_id`", 400);
    };
    let was = match store.clear_pause(trade_id, blackout_id).await {
        Ok(b) => b,
        Err(err) => {
            tracing::error!("KV clear_pause: {err}");
            return ControlResult::error("state error", 500);
        }
    };
    tracing::info!("resume: trade_id={trade_id} blackout_id={blackout_id} was_set={was}");
    let outcome = if was {
        format!("pause-cleared: {blackout_id}")
    } else {
        format!("pause-cleared: {blackout_id} (noop)")
    };
    record_seen(store, verified, now, &outcome).await;
    ControlResult::ok("ok")
}

/// Handle the `news-start` action: open a news window for
/// `(trade_id, news_id)`. No broker work. Mirrors `handle_pause` but
/// writes to the news-window KV namespace, which only the gated
/// `close` reads — entries are not blocked by news windows.
pub async fn handle_news_start<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return ControlResult::error("news-start requires `trade_id`", 400);
    };
    let Some(news_id) = verified.intent.news_id.as_deref() else {
        return ControlResult::error("news-start requires `news_id`", 400);
    };
    // Same TTL math as pause: the matching `news-end` is the
    // authoritative clear; the safety tail is just to stop an
    // orphaned window pinning the trade forever.
    let ttl_seconds = veto_ttl_seconds(0, verified.intent.not_after, now);
    let reason = verified.intent.reason.as_deref();
    if let Err(err) = store
        .set_news_window(trade_id, news_id, reason, now, ttl_seconds)
        .await
    {
        tracing::error!("KV set_news_window: {err}");
        return ControlResult::error("state error", 500);
    }
    record_control_event_for(
        store,
        verified.intent.account.as_deref(),
        Some(trade_id),
        ControlKind::News,
        news_id,
        &verified.intent.instrument,
        ttl_seconds,
        now,
        None,
    )
    .await;
    tracing::info!(
        "news-start: trade_id={trade_id} news_id={news_id} reason={:?}",
        reason
    );
    let outcome = match reason {
        Some(r) => format!("news-start: {news_id} ({r})"),
        None => format!("news-start: {news_id}"),
    };
    record_seen(store, verified, now, &outcome).await;
    ControlResult::ok("ok")
}

/// Handle the `news-end` action: close a single
/// `(trade_id, news_id)` news window.
pub async fn handle_news_end<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return ControlResult::error("news-end requires `trade_id`", 400);
    };
    let Some(news_id) = verified.intent.news_id.as_deref() else {
        return ControlResult::error("news-end requires `news_id`", 400);
    };
    let was = match store.clear_news_window(trade_id, news_id).await {
        Ok(b) => b,
        Err(err) => {
            tracing::error!("KV clear_news_window: {err}");
            return ControlResult::error("state error", 500);
        }
    };
    tracing::info!("news-end: trade_id={trade_id} news_id={news_id} was_set={was}");
    let outcome = if was {
        format!("news-end: {news_id}")
    } else {
        format!("news-end: {news_id} (noop)")
    };
    record_seen(store, verified, now, &outcome).await;
    ControlResult::ok("ok")
}

/// Handle the `register` action: accept a server-side
/// [`TradePlan`](crate::trade_plan::TradePlan) for the engine to
/// evaluate on each cron tick. A control action — no broker work; idempotent
/// (re-registering refreshes the row), so it marks-seen on every completion
/// like the other control handlers.
pub async fn handle_register<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let Some(plan) = verified.intent.trade_plan.as_ref() else {
        return ControlResult::error("register requires a `trade_plan`", 400);
    };
    // The plan and its carrier intent must agree on which trade they describe,
    // otherwise the engine couldn't key the plan's state to the intent's id.
    if let Some(intent_trade_id) = verified.intent.trade_id.as_deref()
        && intent_trade_id != plan.trade_id
    {
        return ControlResult::error(
            "register: intent trade_id does not match plan trade_id",
            400,
        );
    }
    // Persist the plan for the cron engine to evaluate. No TTL: a registered
    // plan never times out (the carrier intent's `not_after` is a control TTL,
    // unrelated to the plan's lifetime). It retires only via the engine's
    // archive path when the plan reaches a terminal state. See the 5-min
    // `CONTROL_TTL` register bug (2026-06-23).
    let account = verified.intent.account.as_deref();
    if let Err(err) = store.put_trade_plan(account, plan).await {
        tracing::error!(
            "register: put_trade_plan failed (trade_id={}): {err}",
            plan.trade_id
        );
        return ControlResult::error("state error", 500);
    }
    tracing::info!(
        "register: trade_id={} instrument={} rules={} persisted (no expiry)",
        plan.trade_id,
        plan.instrument,
        plan.rules.len()
    );
    let outcome = format!(
        "registered: {} ({} rules, persisted)",
        plan.trade_id,
        plan.rules.len()
    );
    record_seen(store, verified, now, &outcome).await;
    ControlResult::ok("ok")
}

/// A compact, operator-facing view of one registered plan + its current engine
/// state. Used by [`handle_plan_list`] — small enough to list many plans
/// without burying the reader in each rule's embedded intent (use `plan show`
/// for the full dump). Serialised to YAML for the `trade-control plan list`
/// response.
#[derive(Serialize)]
struct PlanSummary {
    trade_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    instrument: String,
    granularity: crate::broker::Granularity,
    /// Observe-only? The thing an operator most wants to confirm during the
    /// engine's parallel-run period.
    shadow: bool,
    rules: usize,
    /// `PlanState`-derived fields. `None`/empty until the plan's first cron
    /// tick has seeded its state (a registered-but-not-yet-ticked plan has no
    /// state row).
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<crate::plan_state::Phase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    watermark: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fired: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retest_seen_at: Option<DateTime<Utc>>,
    /// Set only for an archived (terminated) plan — the time the engine archived
    /// it on its terminal cron tick. Absent on a live plan, which doubles as the
    /// CLI's "is this row terminated?" marker (`ARCHIVED` column). Surfaced only
    /// by `plan list --include-all`.
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<DateTime<Utc>>,
}

/// The full `plan show` payload: the whole registered plan plus its engine
/// state, both serialised verbatim so the operator can inspect every rule and
/// the exact persisted state.
#[derive(Serialize)]
pub struct PlanDetail {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub plan: crate::trade_plan::TradePlan,
    /// `None` until the first cron tick seeds the plan's state row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<crate::plan_state::PlanState>,
    /// `Some` when this match came from the archive (a terminated plan), `None`
    /// for a live registered plan. Mirrors [`PlanSummary::archived_at`] so the
    /// operator can tell at a glance whether `plan show` surfaced a live or a
    /// finished plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,
}

/// Handle the `plan-list` action: enumerate every registered plan across all
/// account scopes, pair each with its current `PlanState`, and return a compact
/// YAML summary. Read-only, KV-only, idempotent (marks seen on completion like
/// every other control action).
pub async fn handle_plan_list<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let plans = match store.list_all_trade_plans().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("plan-list: list_all_trade_plans: {err}");
            return ControlResult::error("state error", 500);
        }
    };

    let mut summaries = Vec::with_capacity(plans.len());
    for stored in &plans {
        let state = store
            .get_plan_state(stored.account.as_deref(), &stored.plan.trade_id)
            .await
            .unwrap_or(None);
        summaries.push(plan_summary(stored, state));
    }

    // `--include-all`: also fold in the archived (terminated) plans so a vetoed
    // or completed setup can be analyzed after the engine dropped its live rows.
    if verified.intent.include_archived {
        match store.list_all_archived_plans().await {
            Ok(archived) => summaries.extend(archived.iter().map(archived_plan_summary)),
            Err(err) => {
                tracing::error!("plan-list: list_all_archived_plans: {err}");
                return ControlResult::error("state error", 500);
            }
        }
    }

    // Stable ordering so a re-list is byte-comparable: account, then trade_id.
    // (A live and an archived plan never share a trade_id — the live row is
    // deleted in the same terminal tick that writes the archive.)
    summaries.sort_by(|a, b| {
        a.account
            .cmp(&b.account)
            .then_with(|| a.trade_id.cmp(&b.trade_id))
    });

    let body = match serde_yaml::to_string(&summaries) {
        Ok(s) => s,
        Err(err) => {
            tracing::error!("plan-list serialise: {err}");
            return ControlResult::error("internal error", 500);
        }
    };
    record_seen(
        store,
        verified,
        now,
        &format!("plan-list: {} plans", summaries.len()),
    )
    .await;
    ControlResult::ok(body)
}

/// Build the compact summary for one stored plan + its (optional) state.
fn plan_summary(stored: &StoredPlan, state: Option<crate::plan_state::PlanState>) -> PlanSummary {
    let plan = &stored.plan;
    PlanSummary {
        trade_id: plan.trade_id.clone(),
        account: stored.account.clone(),
        instrument: plan.instrument.clone(),
        granularity: plan.granularity,
        shadow: plan.shadow,
        rules: plan.rules.len(),
        phase: state.as_ref().map(|s| s.phase),
        watermark: state.as_ref().and_then(|s| s.watermark),
        fired: state
            .as_ref()
            .map(|s| s.fired.iter().cloned().collect())
            .unwrap_or_default(),
        retest_seen_at: state.as_ref().and_then(|s| s.retest_seen_at),
        archived_at: None, // live plan — see `archived_plan_summary`
    }
}

/// Build the compact summary for one archived (terminated) plan. The terminal
/// `final_state` is always present (unlike a live plan, which may not have
/// ticked yet), so `phase`/`fired` are taken from it directly. `archived_at`
/// being `Some` is what flags the row as terminated to the CLI.
fn archived_plan_summary(archived: &ArchivedPlan) -> PlanSummary {
    let plan = &archived.plan;
    let state = &archived.final_state;
    PlanSummary {
        trade_id: plan.trade_id.clone(),
        account: archived.account.clone(),
        instrument: plan.instrument.clone(),
        granularity: plan.granularity,
        shadow: plan.shadow,
        rules: plan.rules.len(),
        phase: Some(state.phase),
        watermark: state.watermark,
        fired: state.fired.iter().cloned().collect(),
        retest_seen_at: state.retest_seen_at,
        archived_at: Some(archived.archived_at),
    }
}

/// Gather the full `plan show` detail(s) for one `trade_id` from **both** the
/// live plan rows and the archive. trade_ids are unique in practice, but if two
/// scopes share one we return every match so nothing is hidden. A terminated
/// plan usually exists *only* in the archive (its live rows were dropped on the
/// terminal tick), so scanning the archive is what makes a finished plan
/// (the kind `plan list --include-archived` surfaces) inspectable at all.
///
/// Pure and [`StateStore`]-generic so it's unit-testable off-wasm with a
/// `MemStateStore`; the HTTP response construction stays in the caller.
pub async fn collect_plan_details<S: StateStore>(
    store: &S,
    target: &str,
) -> Result<Vec<PlanDetail>, StateError> {
    let mut details = Vec::new();

    // Live registered plans: pair each match with its current engine state
    // (which may be `None` if the plan hasn't ticked yet).
    for stored in store.list_all_trade_plans().await? {
        if stored.plan.trade_id != target {
            continue;
        }
        let state = store
            .get_plan_state(stored.account.as_deref(), &stored.plan.trade_id)
            .await
            .unwrap_or(None);
        details.push(PlanDetail {
            account: stored.account,
            plan: stored.plan,
            state,
            archived_at: None,
        });
    }

    // Archived (terminated) plans: the terminal `final_state` is always present,
    // and `archived_at` flags the match as a finished plan to the operator.
    for archived in store.list_all_archived_plans().await? {
        if archived.plan.trade_id != target {
            continue;
        }
        details.push(PlanDetail {
            account: archived.account,
            plan: archived.plan,
            state: Some(archived.final_state),
            archived_at: Some(archived.archived_at),
        });
    }

    Ok(details)
}

/// Handle the `plan-show` action: dump one plan in full. The target is named by
/// `intent.trade_id`; we scan every account scope — **live and archived** — and
/// return the match(es) so a finished plan surfaced by `plan list
/// --include-archived` is still inspectable. Returns 404 when no plan matches.
pub async fn handle_plan_show<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let Some(target) = verified.intent.trade_id.as_deref() else {
        return ControlResult::error("plan-show requires a `trade_id`", 400);
    };

    let details = match collect_plan_details(store, target).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("plan-show: collect_plan_details: {err}");
            return ControlResult::error("state error", 500);
        }
    };

    if details.is_empty() {
        record_seen(
            store,
            verified,
            now,
            &format!("plan-show: {target} not found"),
        )
        .await;
        return ControlResult::error(format!("no registered plan with trade_id {target}"), 404);
    }

    let body = match serde_yaml::to_string(&details) {
        Ok(s) => s,
        Err(err) => {
            tracing::error!("plan-show serialise: {err}");
            return ControlResult::error("internal error", 500);
        }
    };
    record_seen(store, verified, now, &format!("plan-show: {target}")).await;
    ControlResult::ok(body)
}

/// Handle the `plan-delete` action: drop a registered plan and its engine
/// state — the inverse of `register`. The target is named by
/// `intent.trade_id`; we scan every account scope (as `plan-show` does) and
/// delete each matching `plan:` + `plan-state:` row, so the operator can
/// re-arm a setup after editing its chart. We **also** clear any matching
/// archived (terminated) plan, so a vetoed/completed plan surfaced by
/// `plan list --include-all` can be dropped after analysis — and so an id that
/// only exists in the archive (the common case: the live rows were already
/// deleted on the terminal tick) is still deletable. Idempotent and KV-only: a
/// delete of a non-existent plan returns `ok` (count 0), never an error —
/// re-running `plan delete` is always safe.
pub async fn handle_plan_delete<S: StateStore>(
    store: &S,
    verified: &Verified,
    now: DateTime<Utc>,
) -> ControlResult {
    let Some(target) = verified.intent.trade_id.as_deref() else {
        return ControlResult::error("plan-delete requires a `trade_id`", 400);
    };

    let plans = match store.list_all_trade_plans().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("plan-delete: list_all_trade_plans: {err}");
            return ControlResult::error("state error", 500);
        }
    };

    // Drop every scope that holds this trade_id. trade_ids are unique in
    // practice, but if two scopes share one we clear both — nothing is left
    // dangling. Each plan carries its own state row, so clear both per match.
    let mut deleted = 0usize;
    for stored in &plans {
        if stored.plan.trade_id != target {
            continue;
        }
        let account = stored.account.as_deref();
        if let Err(err) = store.clear_trade_plan(account, target).await {
            tracing::error!("plan-delete: clear_trade_plan({target}): {err}");
            return ControlResult::error("state error", 500);
        }
        if let Err(err) = store.clear_plan_state(account, target).await {
            tracing::error!("plan-delete: clear_plan_state({target}): {err}");
            return ControlResult::error("state error", 500);
        }
        deleted += 1;
        tracing::info!(
            "plan-delete: trade_id={target} account={} deleted",
            account.unwrap_or("<global>")
        );
    }

    // Also clear any archived copy. A terminated plan usually exists ONLY here
    // (its live rows were deleted on the terminal tick), so this is the path
    // that actually removes it; it counts toward `deleted` so the operator sees
    // a non-zero count.
    let archived = match store.list_all_archived_plans().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("plan-delete: list_all_archived_plans: {err}");
            return ControlResult::error("state error", 500);
        }
    };
    for stored in &archived {
        if stored.plan.trade_id != target {
            continue;
        }
        let account = stored.account.as_deref();
        if let Err(err) = store.clear_archived_plan(account, target).await {
            tracing::error!("plan-delete: clear_archived_plan({target}): {err}");
            return ControlResult::error("state error", 500);
        }
        deleted += 1;
        tracing::info!(
            "plan-delete: trade_id={target} account={} archived-cleared",
            account.unwrap_or("<global>")
        );
    }

    let outcome = if deleted == 0 {
        format!("plan-deleted: {target} (noop)")
    } else {
        format!("plan-deleted: {target} ({deleted})")
    };
    record_seen(store, verified, now, &outcome).await;
    ControlResult::ok(outcome)
}

#[cfg(test)]
mod plan_show_tests {
    //! Pins [`collect_plan_details`]: `plan show` must find a plan whether it
    //! is a live registered plan **or** an archived (terminated) one. The bug
    //! that motivated this: a finished plan surfaced by `plan list
    //! --include-archived` 404'd on `plan show`, because the handler only
    //! scanned live plans.
    use super::*;
    use crate::plan_state::{Phase, PlanState};
    use crate::state::{MemStateStore, StateStore};
    use crate::trade_plan::TradePlan;
    use chrono::TimeZone;

    /// Minimal valid plan — empty rule set is fine, `collect_plan_details`
    /// matches on `trade_id` only.
    fn sample_plan(trade_id: &str) -> TradePlan {
        let json = format!(
            r#"{{"trade_id":"{trade_id}","instrument":"NZD_CHF","direction":"short",
                "granularity":"d1","pip_size":0.0001,"rules":[]}}"#
        );
        serde_json::from_str(&json).expect("sample plan json")
    }

    /// The regression: a plan that exists ONLY in the archive (its live rows
    /// were dropped on the terminal tick) is still found by `plan show`.
    #[test]
    fn archived_only_plan_is_found() {
        let store = MemStateStore::new();
        let archived_at = Utc.with_ymd_and_hms(2026, 6, 18, 22, 45, 21).unwrap();
        let final_state = PlanState::seed(Phase::Done, archived_at);
        pollster::block_on(store.archive_plan(
            None,
            &sample_plan("hs-nzd-chf-d12eb831"),
            &final_state,
            archived_at,
        ))
        .expect("archive");

        let details = pollster::block_on(collect_plan_details(&store, "hs-nzd-chf-d12eb831"))
            .expect("collect");
        assert_eq!(details.len(), 1, "the archived plan must surface");
        let d = &details[0];
        assert_eq!(d.plan.trade_id, "hs-nzd-chf-d12eb831");
        assert_eq!(
            d.archived_at,
            Some(archived_at),
            "archived match must carry archived_at so the operator can tell"
        );
        assert!(
            d.state.is_some(),
            "archived plan carries its terminal state"
        );
    }

    /// No regression: a live registered plan is still found, and is NOT flagged
    /// as archived.
    #[test]
    fn live_plan_is_found_and_not_flagged_archived() {
        let store = MemStateStore::new();
        pollster::block_on(store.put_trade_plan(None, &sample_plan("hs-live-1"))).expect("put");

        let details =
            pollster::block_on(collect_plan_details(&store, "hs-live-1")).expect("collect");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].plan.trade_id, "hs-live-1");
        assert_eq!(
            details[0].archived_at, None,
            "a live plan must not be flagged archived"
        );
    }

    /// An unknown id matches nothing in either store — the caller turns this
    /// empty vec into a 404.
    #[test]
    fn unknown_id_yields_no_details() {
        let store = MemStateStore::new();
        pollster::block_on(store.put_trade_plan(None, &sample_plan("hs-live-1"))).expect("put");
        let details = pollster::block_on(collect_plan_details(&store, "nope")).expect("collect");
        assert!(details.is_empty());
    }
}
