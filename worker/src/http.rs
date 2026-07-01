//! The native axum HTTP receiver — the VM replacement for the Cloudflare
//! Worker's `#[event(fetch)]` (`src/lib.rs`).
//!
//! It receives a signed TradingView alert over **plain HTTP on loopback** (TLS
//! terminated by a reverse proxy), verifies + parses it through the *shared*
//! [`parse_and_verify`], runs the *shared* replay-protection and dispatch
//! (`trade_control_core::dispatch::*`) against [`PgStateStore`], and maps the
//! outcome to an HTTP response. The flow mirrors the wasm worker's `'intent`
//! loop (`src/lib.rs` ~108-320) so the two edges can't drift.
//!
//! ## Why a local-thread dispatcher
//!
//! The shared [`Broker`](trade_control_core::broker::Broker) trait returns
//! `?Send` futures (the broker SDKs hold `!Send` reqwest clients — the codebase
//! targets Cloudflare's single-threaded executor). axum's `Handler` bound
//! requires the handler future to be **`Send`**, so the broker dispatch can't
//! run directly inside an axum handler on the multi-thread runtime.
//!
//! [`Dispatcher`] bridges that: a dedicated OS thread runs a *current-thread*
//! tokio runtime with a [`LocalSet`], owns the [`AppState`], and processes one
//! request at a time via [`spawn_local`]. The axum handler is a thin **`Send`**
//! shim — it ships the request body over a channel and awaits the `(status,
//! body)` reply (both `Send`), while all the `!Send` work stays on the local
//! thread. Single-threaded processing also matches the worker's one-alert-at-a-
//! time semantics (the seen-id / retry-gate state assumes no concurrent fire of
//! the same id).
//!
//! ## Routes
//! * `POST /` — the signed-alert webhook (the only request-processing route).
//! * `GET /health` — liveness probe: a cheap `200 OK` proving the process is up
//!   and serving, so a proxy/uptime check needn't POST a garbage body and read a
//!   4xx. Deliberately liveness, not readiness — it does **no** DB round-trip, so
//!   it stays green during a transient Postgres blip rather than flapping the
//!   whole service out of the proxy. (A future readiness probe that pings the
//!   pool can live at a separate path.)
//!
//! ## Not here yet (deliberate)
//! * `PlanPurge` / `PurgeOlderThan` / `MarketInfo` need R2 (the tick bundles)
//!   and the TradeNation market-info glue the wasm worker has; natively they
//!   return `501 Not Implemented` until that glue lands (a later task).
//! * Global / default-account routing for a `None`-account broker intent is a
//!   follow-up — such an intent currently returns `400 account required`.

use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use tokio::sync::{mpsc, oneshot};
use trade_control_core::account::{AccountMetadata, MetadataError, MetadataStore};
use trade_control_core::dispatch::{
    ActionResult, ControlResult, handle_clear_prep, handle_clear_veto, handle_news_end,
    handle_news_start, handle_pause, handle_plan_delete, handle_plan_list, handle_plan_show,
    handle_prep, handle_prep_expire, handle_register, handle_resume, handle_status, handle_unlock,
    handle_veto, is_multishot_enter, record_dispatcher_outcome, record_seen, run_action,
};
use trade_control_core::incoming::{IncomingDisposition, Verified, parse_and_verify};
use trade_control_core::intent::{Action, BrokerKind, VetoLevel};
use trade_control_core::state::StateStore;

use crate::dispatch_config_native::build_dispatch_config_native;
use crate::{PgMetadataStore, PgStateStore, Secrets, acquire_oanda, acquire_tn};

/// Shared application state owned by the local dispatcher thread. The HMAC
/// `signing_key` is stored **already hex-decoded** (the wire key is hex; the
/// wasm worker decodes it once via `sig::parse_key_hex`) so the per-request path
/// doesn't re-parse it.
pub struct AppState {
    pub store: PgStateStore,
    pub accounts: PgMetadataStore,
    pub secrets: Secrets,
    /// The hex-decoded HMAC verification key.
    pub signing_key: Vec<u8>,
}

/// One unit of work shipped from an axum handler to the local dispatcher thread:
/// the raw request body plus a one-shot reply channel for the `(status, body)`
/// the handler will turn into an HTTP response.
struct Job {
    body: String,
    reply: oneshot::Sender<(StatusCode, String)>,
}

/// `Send` handle to the local dispatcher thread. Cloning it is cheap (an `mpsc`
/// sender clone); the axum router holds it as state.
#[derive(Clone)]
pub struct Dispatcher {
    tx: mpsc::UnboundedSender<Job>,
}

impl Dispatcher {
    /// Spawn the dispatcher thread that owns `state` and processes requests on a
    /// current-thread runtime + [`LocalSet`], so the `!Send` broker dispatch is
    /// legal. Returns a `Send` handle the axum handlers clone.
    ///
    /// The thread runs for the process lifetime; when the last [`Dispatcher`]
    /// handle drops, the channel closes and the run-loop exits.
    pub fn spawn(state: Arc<AppState>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        std::thread::Builder::new()
            .name("tc-dispatch".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!("dispatcher runtime build failed: {e}");
                        return;
                    }
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    while let Some(job) = rx.recv().await {
                        let state = state.clone();
                        // One request at a time: `spawn_local` + immediate await
                        // keeps the single-flight semantics the seen-id / retry
                        // gate assume, while staying on the local thread.
                        let handle = tokio::task::spawn_local(async move {
                            let (status, text) = dispatch_request(&state, &job.body).await;
                            let _ = job.reply.send((status, text));
                        });
                        if let Err(e) = handle.await {
                            tracing::error!("dispatch task panicked: {e}");
                        }
                    }
                });
            })
            .map(|_| ())
            .unwrap_or_else(|e| tracing::error!("failed to spawn dispatcher thread: {e}"));
        Self { tx }
    }

    /// Hand a request body to the local thread and await its `(status, body)`.
    async fn dispatch(&self, body: String) -> (StatusCode, String) {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Job { body, reply }).is_err() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "dispatcher unavailable".to_string(),
            );
        }
        match rx.await {
            Ok(parts) => parts,
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "dispatcher dropped".to_string(),
            ),
        }
    }
}

/// Build the axum router:
/// * `POST /` — the signed-alert webhook.
/// * `GET /health` — liveness probe (cheap `200 OK`, no DB round-trip).
///
/// Diagnostic / admin routes (`/diag/*`, `/admin/*`) the wasm worker also
/// served are not part of this native receiver task — they'll be added with the
/// native admin surface.
pub fn router(dispatcher: Dispatcher) -> Router {
    Router::new()
        .route("/", post(webhook))
        .route("/health", get(health))
        .with_state(dispatcher)
}

/// Liveness probe — a `Send` handler that returns `200 OK` without touching the
/// dispatcher thread or Postgres. Proves the process is up and the axum server
/// is accepting connections, which is what a proxy/uptime check needs. It is
/// deliberately **not** readiness: it does no DB ping, so a transient Postgres
/// blip won't flap the worker out of the proxy's pool (the request path itself
/// fails loudly per-request if the DB is down). The `_dispatcher` state is unused
/// but present because the router is typed `Router<()>` after `with_state`.
async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// The axum webhook handler — a thin `Send` shim. It captures the request body
/// and hands it to the local dispatcher thread, which does the `!Send` work.
async fn webhook(State(dispatcher): State<Dispatcher>, body: String) -> Response {
    let (status, text) = dispatcher.dispatch(body).await;
    (status, text).into_response()
}

/// Process one signed-alert request to a `(status, body)`. Mirrors the wasm
/// worker's `'intent` loop. Runs on the local dispatcher thread (so the `!Send`
/// broker futures are legal). The request recorder fires on every return path,
/// the way the wasm worker records every `'intent` outcome to R2 — here it
/// inserts a row into the Postgres `request_records` table instead.
async fn dispatch_request(state: &Arc<AppState>, body: &str) -> (StatusCode, String) {
    let now = Utc::now();
    let parts = dispatch_inner(state, body, now).await;
    // Record off the response's critical path: the recorder is fire-and-forget
    // (matches the tick recorder + the wasm worker's `wait_until`), so the
    // response returns immediately and the next single-flight request isn't held
    // behind this insert. The spawned task logs the response + whether the insert
    // landed, so the trail stays visible.
    record_request(state, body, now, &parts);
    parts
}

/// The actual decision logic, factored out so [`dispatch_request`] can wrap
/// every return with the request recorder.
async fn dispatch_inner(
    state: &AppState,
    body: &str,
    now: chrono::DateTime<Utc>,
) -> (StatusCode, String) {
    // --- Parse + verify (shared). ---
    let verified = match parse_and_verify(body, &state.signing_key, now) {
        Ok(v) => v,
        Err(err) => {
            return match err.disposition() {
                // A benign time-window decline is a well-formed, correctly-
                // signed intent that fired outside its [not_before, not_after]
                // window — 200 with a distinct `declined:` outcome, same as the
                // wasm worker (bug #9).
                IncomingDisposition::DeclinedExpired => {
                    tracing::info!("incoming declined: {err} | body_len={}", body.len());
                    (StatusCode::OK, "declined: intent-expired".to_string())
                }
                IncomingDisposition::DeclinedTooEarly => {
                    tracing::info!("incoming declined: {err} | body_len={}", body.len());
                    (StatusCode::OK, "declined: intent-too-early".to_string())
                }
                IncomingDisposition::Rejected => {
                    tracing::error!("incoming rejected: {err} | body_len={}", body.len());
                    (StatusCode::BAD_REQUEST, "rejected".to_string())
                }
            };
        }
    };

    // --- Replay protection (shared multi-shot exception). ---
    match state.store.is_seen(&verified.intent.id).await {
        // A multi-shot enter re-fires the same baked id every signal bar; the
        // retry gate (inside `run_enter`) is its replay authority. Fall through.
        Ok(true) if is_multishot_enter(&verified.intent) => {
            tracing::info!(
                "seen multi-shot enter id={} — deferring replay decision to retry gate",
                verified.intent.id
            );
        }
        Ok(true) => return (StatusCode::CONFLICT, "replay".to_string()),
        Ok(false) => {}
        Err(err) => {
            tracing::error!("is_seen: {err}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "state error".to_string());
        }
    }

    // --- Control actions (no broker). ---
    if let Some(result) = dispatch_control(state, &verified, now).await {
        return control_to_parts(result);
    }

    // --- Broker actions (Enter / Close / Invalidate / escalated Veto). ---
    dispatch_broker(state, &verified, body, now).await
}

/// Dispatch the control actions that don't touch the broker. Returns
/// `Some(result)` when this intent was a control action handled here, `None`
/// when it's a broker action (or an escalated `Veto`) that must fall through to
/// [`dispatch_broker`].
///
/// `PlanPurge` / `PurgeOlderThan` / `MarketInfo` are recognised but **not yet
/// supported natively** (they need R2 / TradeNation-market glue) — they return
/// a `501` control result rather than falling through.
async fn dispatch_control(
    state: &AppState,
    verified: &Verified,
    now: chrono::DateTime<Utc>,
) -> Option<ControlResult> {
    let store = &state.store;
    let r = match verified.intent.action {
        Action::Status => handle_status(store, verified, now).await,
        Action::Unlock => handle_unlock(store, verified, now).await,
        Action::Prep => handle_prep(store, verified, now).await,
        Action::PrepExpire => handle_prep_expire(store, verified, now).await,
        Action::Veto => {
            // Only a `stop-next-entry` veto is broker-free; higher levels cancel
            // resting orders / close positions and fall through to the broker.
            if matches!(
                verified.intent.level.unwrap_or_default(),
                VetoLevel::StopNextEntry
            ) {
                handle_veto(store, verified, now).await
            } else {
                return None;
            }
        }
        Action::ClearPrep => handle_clear_prep(store, verified, now).await,
        Action::ClearVeto => handle_clear_veto(store, verified, now).await,
        Action::Pause => handle_pause(store, verified, now).await,
        Action::Resume => handle_resume(store, verified, now).await,
        Action::NewsStart => handle_news_start(store, verified, now).await,
        Action::NewsEnd => handle_news_end(store, verified, now).await,
        Action::Register => handle_register(store, verified, now).await,
        Action::PlanList => handle_plan_list(store, verified, now).await,
        Action::PlanShow => handle_plan_show(store, verified, now).await,
        // Recording-backed (reads `request_records`), so it can't be a generic
        // `core` handler like the others — it's dispatched here against the
        // concrete `PgStateStore` pool, the same way the R2-backed purge arms
        // are worker-local.
        Action::PlanTimeline => handle_plan_timeline(store, verified, now).await,
        Action::PlanDelete => handle_plan_delete(store, verified, now).await,
        // Not yet supported natively — these need R2 (the `ticks/` bundles for
        // purge) or the TradeNation market-info call (MarketInfo). The wasm
        // worker dispatches them with `Env` in scope; the native glue is a
        // later task. Reject loudly rather than silently doing nothing.
        Action::PlanPurge | Action::PurgeOlderThan | Action::MarketInfo => ControlResult::error(
            "not supported on the native runtime yet (needs R2 / market-info glue)",
            501,
        ),
        // Everything else (Enter / Close / Invalidate) is a broker action.
        _ => return None,
    };
    Some(r)
}

/// `plan timeline <trade_id>` — reconstruct the event timeline for one trade
/// from the durable recordings. The worker-local counterpart of the generic
/// `handle_plan_show`: because the read hits the concrete Postgres pool
/// (recordings are not part of the `StateStore` trait), it lives here rather
/// than in `core`.
///
/// A trade's life spans two event streams, so we read **both**: the inbound
/// signed-alert [`RequestRecord`]s (`request_records`) and the cron-engine
/// [`TickBundle`]s (`tick_bundles`, keyed on `correlation_id == trade_id`). A
/// veto or enter that fired on a cron tick — the common case now — shows up
/// only in the tick stream, so a records-only timeline would miss it.
///
/// Returns a [`PlanTimeline`] envelope as YAML on `Ok`, a 404 when the trade
/// has neither stream, or a 400 when `trade_id` is missing. Recorded as seen on
/// completion (idempotent read, like `plan-show`).
async fn handle_plan_timeline(
    store: &PgStateStore,
    verified: &Verified,
    now: chrono::DateTime<Utc>,
) -> ControlResult {
    let Some(target) = verified.intent.trade_id.as_deref() else {
        return ControlResult::error("plan-timeline requires a `trade_id`", 400);
    };

    let records = match crate::recording_pg::request_records_for_trade(store.pool(), target).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("plan-timeline: request_records_for_trade: {err}");
            return ControlResult::error("state error", 500);
        }
    };
    let ticks = match crate::recording_pg::tick_bundles_for_trade(store.pool(), target).await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("plan-timeline: tick_bundles_for_trade: {err}");
            return ControlResult::error("state error", 500);
        }
    };

    let timeline = trade_control_core::recording::PlanTimeline { records, ticks };

    if timeline.is_empty() {
        record_seen(
            store,
            verified,
            now,
            &format!("plan-timeline: {target} not found"),
        )
        .await;
        return ControlResult::error(format!("no recorded events for trade_id {target}"), 404);
    }

    let body = match serde_yaml::to_string(&timeline) {
        Ok(s) => s,
        Err(err) => {
            tracing::error!("plan-timeline serialise: {err}");
            return ControlResult::error("internal error", 500);
        }
    };
    record_seen(store, verified, now, &format!("plan-timeline: {target}")).await;
    ControlResult::ok(body)
}

/// Dispatch a broker action (Enter / Close / Invalidate / escalated Veto)
/// against the account's broker, then map the [`ActionResult`] to a response.
/// Records the dispatcher outcome on the seen index (shared helper).
async fn dispatch_broker(
    state: &AppState,
    verified: &Verified,
    body: &str,
    now: chrono::DateTime<Utc>,
) -> (StatusCode, String) {
    // Resolve the account metadata. A named account that isn't in the index is a
    // 400; an unnamed account with a broker intent has no credentials to route
    // to (global/default-account routing is a follow-up) — also a 400.
    let meta = match resolve_account(state, verified).await {
        Ok(m) => m,
        Err(AccountResolveError::Unknown(name)) => {
            tracing::error!("unknown account '{name}'");
            return (StatusCode::BAD_REQUEST, "unknown account".to_string());
        }
        Err(AccountResolveError::Required) => {
            // TODO: global/default-account routing for an unnamed broker intent.
            tracing::error!("broker intent without a named account — no default routing yet");
            return (StatusCode::BAD_REQUEST, "account required".to_string());
        }
        Err(AccountResolveError::Backend(msg)) => {
            tracing::error!("account lookup failed: {msg}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "state error".to_string());
        }
    };

    let cfg = build_dispatch_config_native(&state.secrets, &verified.intent.instrument, meta.caps);

    // Branch on the broker kind exactly like the wasm worker: build the concrete
    // broker, then monomorphize the shared `run_action` per arm. On acquire
    // failure, mirror the worker's statuses (OANDA → 500, TradeNation → 503).
    let result = match verified.intent.broker {
        BrokerKind::Oanda => match acquire_oanda(&meta, &state.secrets) {
            Ok(broker) => run_action(&broker, &state.store, verified, &cfg, now, body).await,
            Err(err) => {
                tracing::error!("oanda acquire failed: {err}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "oanda login failed".to_string(),
                );
            }
        },
        BrokerKind::TradeNation => match acquire_tn(&meta).await {
            Ok(broker) => run_action(&broker, &state.store, verified, &cfg, now, body).await,
            Err(err) => {
                tracing::error!("tradenation acquire failed: {err}");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "tradenation login failed (missing account, bad credentials, or expired \
                     session — check logs)"
                        .to_string(),
                );
            }
        },
    };

    // Shared seen-index write (only `Ok` marks; `Failed`/`Rejected` log only).
    record_dispatcher_outcome(&state.store, verified, now, &result).await;
    action_to_parts(&result)
}

/// Account-resolution failure modes at the native edge.
enum AccountResolveError {
    /// The intent named an account that isn't in the index.
    Unknown(String),
    /// The intent named no account but routes to a broker that needs one.
    Required,
    /// The account index lookup itself failed.
    Backend(String),
}

/// Resolve the [`AccountMetadata`] for a broker intent. A named account must
/// exist; an unnamed account has no credentials to route to yet.
async fn resolve_account(
    state: &AppState,
    verified: &Verified,
) -> Result<AccountMetadata, AccountResolveError> {
    match verified.intent.account.as_deref() {
        Some(name) => state.accounts.get(name).await.map_err(|e| match e {
            MetadataError::NotFound(n) => AccountResolveError::Unknown(n),
            other => AccountResolveError::Backend(other.to_string()),
        }),
        None => Err(AccountResolveError::Required),
    }
}

/// Map a control-action [`ControlResult`] to an HTTP `(status, body)`.
fn control_to_parts(c: ControlResult) -> (StatusCode, String) {
    let status = StatusCode::from_u16(c.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, c.body)
}

/// Map a broker [`ActionResult`] to an HTTP `(status, body)`, matching the wasm
/// worker (`Ok` → 200 "ok", `Failed` → 502 "action failed", `Rejected` → its
/// own status + body).
fn action_to_parts(result: &ActionResult) -> (StatusCode, String) {
    match result {
        ActionResult::Ok(_) => (StatusCode::OK, "ok".to_string()),
        ActionResult::Failed(_) => (StatusCode::BAD_GATEWAY, "action failed".to_string()),
        ActionResult::Rejected { status, body, .. } => (
            StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            body.clone(),
        ),
    }
}

/// Build the [`RequestRecord`] for this request and insert it into the Postgres
/// `request_records` table. Fail-soft: any insert error is logged and swallowed
/// — recording must never fail a request.
///
/// Simplifications vs the wasm worker's R2 record (acceptable here; the
/// *decisions* are what matter, and they're captured):
/// * **headers / method / path** — the native receiver doesn't thread the axum
///   request parts down to the dispatcher (the handler ships only the body over
///   the channel). We record `method: "POST"`, `path: "/"` (the only route) and
///   an empty header vector. `request_id` is still minted from the body alone
///   (`&[]` headers), which is deterministic for a given body.
/// * **logs** — native `tracing` isn't buffered per-request yet (the wasm
///   worker's thread-local `LOG_BUFFER` is a Cloudflare single-thread-per-
///   request artifact). We record `logs: vec![]`; per-request log capture is a
///   follow-up.
/// * **outcome** — the dispatch response body doubles as the short outcome
///   string (e.g. `"ok"`, `"replay"`, `"rejected"`, `"declined: …"`), exactly
///   the strings the `*_to_parts` mappers produce.
fn record_request(
    state: &Arc<AppState>,
    body: &str,
    now: chrono::DateTime<Utc>,
    parts: &(StatusCode, String),
) {
    use trade_control_core::recording::{RequestRecord, ids_from_body, mint_request_id};

    let (status, outcome) = parts;
    let (intent_id, trade_id) = ids_from_body(body);
    let record = RequestRecord {
        ts: now.to_rfc3339(),
        request_id: mint_request_id(body, &[]),
        method: "POST".to_string(),
        path: "/".to_string(),
        headers: vec![],
        body: body.to_string(),
        intent_id,
        trade_id,
        status: status.as_u16(),
        outcome: outcome.clone(),
        logs: vec![],
    };

    // Fire-and-forget on the dispatcher's `LocalSet` (the request path is `?Send`
    // so we're already on a local-thread runtime — see `Dispatcher`). The task
    // owns the record, so the response returns without waiting on the insert. It
    // logs the response (status + outcome + request_id) on success and the error
    // on failure, so every recorded request still leaves a trail. Recording can
    // never break the request path — a failed insert is logged + swallowed here.
    let state = state.clone();
    tokio::task::spawn_local(async move {
        let request_id = record.request_id.clone();
        let status = record.status;
        let outcome = record.outcome.clone();
        match crate::recording_pg::record_request(state.store.pool(), &record).await {
            Ok(()) => tracing::info!(
                "recording: recorded request id={request_id} status={status} outcome={outcome:?}"
            ),
            Err(err) => tracing::error!(
                "recording: request_records insert failed (id={request_id} status={status} \
                 outcome={outcome:?}): {err}"
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // These handler unit tests cover the *pre-store* edges (parse/verify) that
    // don't need Postgres:
    //   * a malformed body → the 400 `rejected` branch
    //   * a body signed under a different key → the 400 `rejected` branch
    // The store-touching paths (replay, control, broker) need a live Postgres /
    // broker and are exercised by integration tests + manual runs (Task #6).
    //
    // We can't build a real `AppState` without a `PgStateStore` (it needs a live
    // pool), so these tests assert that `parse_and_verify` — the handler's first
    // branch — rejects bad input with the disposition the handler maps to a 400.
    // The `*_to_parts` mappers are exercised directly.

    fn key() -> Vec<u8> {
        // 32-byte hex key decoded to raw bytes (what AppState stores).
        trade_control_core::sig::parse_key_hex(
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        )
        .unwrap()
        .to_vec()
    }

    #[test]
    fn malformed_body_is_rejected() {
        let now = Utc::now();
        let err = parse_and_verify("this is not a signed intent", &key(), now).unwrap_err();
        assert_eq!(
            err.disposition(),
            IncomingDisposition::Rejected,
            "a malformed body must map to the 400 `rejected` branch",
        );
    }

    #[test]
    fn body_signed_under_wrong_key_is_rejected() {
        // A body whose signature doesn't verify under our key → Rejected → 400.
        let now = Utc::now();
        let body = "v: 1\nid: x\naction: status\ninstrument: EUR_USD\nsig: deadbeef\n";
        let err = parse_and_verify(body, &key(), now).unwrap_err();
        assert_eq!(err.disposition(), IncomingDisposition::Rejected);
    }

    #[tokio::test]
    async fn health_returns_200_ok_without_state() {
        // Liveness probe is state-free — it must answer 200 without a pool or
        // dispatcher, so a proxy health check works even before/around a DB blip.
        let resp = health().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn control_result_maps_to_its_status() {
        let (status, body) = control_to_parts(ControlResult::ok("snapshot"));
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "snapshot");

        let (status, _) = control_to_parts(ControlResult::error("bad", 400));
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // The 501 we emit for PlanPurge/PurgeOlderThan/MarketInfo.
        let (status, _) = control_to_parts(ControlResult::error("nope", 501));
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    }

    #[test]
    fn action_result_maps_like_the_wasm_worker() {
        let (s, b) = action_to_parts(&ActionResult::Ok("entered".into()));
        assert_eq!((s, b.as_str()), (StatusCode::OK, "ok"));

        let (s, b) = action_to_parts(&ActionResult::Failed("broker 500".into()));
        assert_eq!((s, b.as_str()), (StatusCode::BAD_GATEWAY, "action failed"));

        let (s, b) = action_to_parts(&ActionResult::Rejected {
            status: 412,
            body: "veto-active (reversal)".into(),
            outcome: "rejected: veto".into(),
        });
        assert_eq!(s, StatusCode::PRECONDITION_FAILED);
        assert_eq!(b, "veto-active (reversal)");
    }
}
