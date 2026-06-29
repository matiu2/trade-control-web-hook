// `recording` is declared first and with `#[macro_use]` so the `rlog!` /
// `rlog_err!` macros it defines are in scope for every module below
// without a per-file `use`. These replace the worker's bare
// `console_log!` / `console_error!` and additionally buffer each line
// into the per-request R2 record.
#[macro_use]
mod recording;

mod accounts;
mod admin;
mod adopt;
mod cron;
mod diag;
mod market_info;
mod r2_purge;
mod spread_blackout;
mod state;
mod tick_recording;
#[cfg(target_arch = "wasm32")]
mod tn_login;
mod tn_login_helpers;
mod tracing_console;
mod tradenation_adapter;

use chrono::Utc;
use worker::{Context, Env, Method, Request, Response, Result, event};

use crate::state::KvStateStore;
use crate::tradenation_adapter::TradeNationAdapter;
#[cfg(target_arch = "wasm32")]
use broker_oanda::login_with_account_id as oanda_login_with;
use broker_oanda::{OandaBroker, login as oanda_login};
// Re-exported at the crate root so existing `crate::run_enter` /
// `crate::ActionResult` call sites in `cron/engine.rs` and
// `cron/blackout_restore.rs` resolve unchanged after the dispatch move to core.
pub(crate) use trade_control_core::dispatch::{
    ActionResult, ControlResult, run_action, run_close, run_enter, run_invalidate,
    run_veto_with_broker,
};
// Replay-protection helpers now live in `core::dispatch::seen` so the wasm
// worker and the native axum receiver share the same logic (and can't drift —
// `[[strategy_changes_in_both_replayer_and_worker]]`). Re-exported at the crate
// root so `fetch`'s call sites (`is_multishot_enter`, `record_dispatcher_outcome`)
// resolve unchanged.
pub(crate) use trade_control_core::dispatch::{is_multishot_enter, record_dispatcher_outcome};
// The 15 worker-free control-action handlers (status / prep / veto / register /
// plan-*) now live in `core::dispatch::control` so the wasm worker and any
// native runtime share the same logic. Re-exported at the crate root so the
// `handle_status(...)` / `handle_plan_show(...)` call sites in `main`'s fetch
// loop and `cron/engine.rs` resolve unchanged. `record_seen` is re-exported too
// because `market_info` and the in-worker `handle_plan_purge` call it.
pub(crate) use trade_control_core::dispatch::{
    handle_clear_prep, handle_clear_veto, handle_news_end, handle_news_start, handle_pause,
    handle_plan_delete, handle_plan_list, handle_plan_show, handle_prep, handle_prep_expire,
    handle_register, handle_resume, handle_status, handle_unlock, handle_veto, record_seen,
};
use trade_control_core::incoming::{self, IncomingDisposition, parse_and_verify};
use trade_control_core::intent::{Action, BrokerKind, VetoLevel};
use trade_control_core::sig;
use trade_control_core::state::StateStore;

pub(crate) const SIGNING_KEY_SECRET: &str = "SIGNING_KEY";
const MAX_RISK_PCT_PER_TRADE_SECRET: &str = "MAX_RISK_PCT_PER_TRADE";
const MAX_OPEN_POSITIONS_SECRET: &str = "MAX_OPEN_POSITIONS";
const PIP_SIZE_SECRET_PREFIX: &str = "PIP_SIZE_";
pub(crate) const KV_NAMESPACE: &str = "TRADE_CONTROL_KV";

/// Default pip size when no `PIP_SIZE_<INSTRUMENT>` secret is set. EUR_USD's
/// pip size; works for most majors. JPY pairs / indices need an override.
const DEFAULT_PIP_SIZE: f64 = 0.0001;

/// Trim a rejected body for the error log. Bodies are cleartext YAML
/// already (the wire format puts them in CF's request log too), so the
/// exposure cost is zero — but we still cap length to keep one log line
/// manageable. 800 chars covers a typical enter payload comfortably.
fn body_excerpt(yaml: &str) -> String {
    const MAX: usize = 800;
    if yaml.len() <= MAX {
        yaml.to_string()
    } else {
        format!("{}…[truncated {} bytes]", &yaml[..MAX], yaml.len() - MAX)
    }
}

#[event(fetch)]
pub async fn main(mut req: Request, env: Env, ctx: Context) -> Result<Response> {
    tracing_console::ConsoleSubscriber::install();
    recording::begin();

    // Diagnostic routes — GET-only, gated by X-Diag-Key. Handled before
    // we consume the body so the body parser doesn't apply. Not recorded
    // (read-only diagnostics, not trades).
    if req.method() == Method::Get {
        let path = req.path();
        return match path.as_str() {
            "/diag/fx" => diag::handle_fx(&req, &env).await,
            "/diag/candles" => diag::handle_candles(&req, &env).await,
            "/admin/accounts" => admin::handle_list(&req, &env).await,
            _ => Response::error("not found", 404),
        };
    }

    // Admin write routes — POST/DELETE, gated by X-Admin-Key. Handled
    // before the intent body parser because their bodies (JSON for
    // POST) don't follow the signed-body shape. Not recorded.
    let path = req.path();
    if path.starts_with("/admin/") {
        return route_admin(&mut req, &env, &path).await;
    }

    // --- Intent path: capture inputs, dispatch, record to R2. ---
    let method = req.method().to_string();
    let headers: Vec<(String, String)> = req.headers().entries().collect();
    let yaml = req.text().await?;
    let request_id = recording::mint_request_id(&yaml, &headers);
    let received_ts = Utc::now().to_rfc3339();
    rlog!("request_id={request_id}");

    // Dispatch the intent. All inner `return`s land in `resp` so we can
    // record the outcome on every path. `'intent` lets the existing
    // early-returns become `break`s with minimal churn.
    let resp: Result<Response> = 'intent: {
        let key = match signing_key(&env) {
            Some(k) => k,
            None => break 'intent Response::error("server misconfigured", 500),
        };

        let now = Utc::now();
        let verified = match parse_and_verify(&yaml, &key, now) {
            Ok(v) => v,
            // A benign time-window decline (`Expired` / `TooEarly`) is a
            // well-formed, correctly-signed intent that simply fired outside
            // its `[not_before, not_after]` window — the *expected*
            // end-of-life outcome for any scheduled alert that keeps firing
            // past its intent's lifetime. Report it as a 200 with a distinct
            // `declined:` outcome so the timeline/verdict downstream can tell
            // it apart from a genuinely malformed/forged request (which stays
            // a 400 `rejected`). `StaleShellTime` is *not* folded in here — a
            // >24h-old plaintext `time` smells of replay. Same status-code
            // convention as bug #7's `declined: mw-not-armed`, here at the
            // parse/verify gate. Bug #9.
            Err(err) => match err.disposition() {
                IncomingDisposition::DeclinedExpired => {
                    rlog!(
                        "incoming declined: {err} | body_len={} body_excerpt={:?}",
                        yaml.len(),
                        body_excerpt(&yaml)
                    );
                    break 'intent Response::ok("declined: intent-expired");
                }
                IncomingDisposition::DeclinedTooEarly => {
                    rlog!(
                        "incoming declined: {err} | body_len={} body_excerpt={:?}",
                        yaml.len(),
                        body_excerpt(&yaml)
                    );
                    break 'intent Response::ok("declined: intent-too-early");
                }
                IncomingDisposition::Rejected => {
                    rlog_err!(
                        "incoming rejected: {err} | body_len={} body_excerpt={:?}",
                        yaml.len(),
                        body_excerpt(&yaml)
                    );
                    break 'intent Response::error("rejected", 400);
                }
            },
        };

        let store = match env.kv(KV_NAMESPACE) {
            Ok(kv) => KvStateStore::new(kv),
            Err(err) => {
                rlog_err!("missing KV namespace {KV_NAMESPACE}: {err:?}");
                break 'intent Response::error("server misconfigured", 500);
            }
        };

        // Replay protection.
        match store.is_seen(&verified.intent.id).await {
            // A seen id normally 409s here. Exception: a multi-shot `enter`
            // re-fires the *same* baked-in intent id on every signal bar, so
            // 409ing it here would block every legitimate re-entry after the
            // first fill closed. Fall through and let `run_enter` →
            // `retry_gate::evaluate` be the replay authority — it dedups true
            // same-bar re-fires on `shell.time` and rejects 412 when a prior
            // attempt is still open. See `is_multishot_enter`.
            Ok(true) if is_multishot_enter(&verified.intent) => {
                rlog!(
                    "seen multi-shot enter id={} — deferring replay decision to retry gate",
                    verified.intent.id
                );
            }
            Ok(true) => break 'intent Response::error("replay", 409),
            Ok(false) => {}
            Err(err) => {
                rlog_err!("KV is_seen: {err}");
                break 'intent Response::error("state error", 500);
            }
        }

        // Control actions don't touch the broker; handle them up front so we
        // don't waste a broker login on them. The exception is a `veto` with
        // `level` above `stop-next-entry` — those cancel pending orders or
        // close positions, which need broker auth, so they fall through to
        // the broker dispatch below.
        match verified.intent.action {
            Action::Status => {
                break 'intent control_to_response(handle_status(&store, &verified, now).await);
            }
            Action::Unlock => {
                break 'intent control_to_response(handle_unlock(&store, &verified, now).await);
            }
            Action::Prep => {
                break 'intent control_to_response(handle_prep(&store, &verified, now).await);
            }
            Action::PrepExpire => {
                break 'intent control_to_response(
                    handle_prep_expire(&store, &verified, now).await,
                );
            }
            Action::Veto => {
                if matches!(
                    verified.intent.level.unwrap_or_default(),
                    VetoLevel::StopNextEntry
                ) {
                    break 'intent control_to_response(handle_veto(&store, &verified, now).await);
                }
                // Higher-level vetos need the broker; fall through.
            }
            Action::ClearPrep => {
                break 'intent control_to_response(handle_clear_prep(&store, &verified, now).await);
            }
            Action::ClearVeto => {
                break 'intent control_to_response(handle_clear_veto(&store, &verified, now).await);
            }
            Action::Pause => {
                break 'intent control_to_response(handle_pause(&store, &verified, now).await);
            }
            Action::Resume => {
                break 'intent control_to_response(handle_resume(&store, &verified, now).await);
            }
            Action::NewsStart => {
                break 'intent control_to_response(handle_news_start(&store, &verified, now).await);
            }
            Action::NewsEnd => {
                break 'intent control_to_response(handle_news_end(&store, &verified, now).await);
            }
            Action::Register => {
                break 'intent control_to_response(handle_register(&store, &verified, now).await);
            }
            Action::PlanList => {
                break 'intent control_to_response(handle_plan_list(&store, &verified, now).await);
            }
            Action::PlanShow => {
                break 'intent control_to_response(handle_plan_show(&store, &verified, now).await);
            }
            Action::PlanDelete => {
                break 'intent control_to_response(
                    handle_plan_delete(&store, &verified, now).await,
                );
            }
            // PlanPurge / PurgeOlderThan touch R2 (the `env`), so — like
            // MarketInfo — they're dispatched here with `env` in scope rather
            // than through the broker-less `run_action`.
            Action::PlanPurge => {
                break 'intent control_to_response(
                    handle_plan_purge(&store, &env, &verified, now).await,
                );
            }
            Action::PurgeOlderThan => {
                break 'intent control_to_response(
                    handle_purge_older_than(&env, &verified, now).await,
                );
            }
            _ => {}
        }

        // `market-info` is a read-only query that needs a live TradeNation
        // broker (its `market_info` call is not on the generic `Broker`
        // trait), so it acquires the broker here and returns its own
        // Response directly — it is not an `ActionResult` and so skips
        // `run_action` / `record_dispatcher_outcome`.
        if verified.intent.action == Action::MarketInfo {
            break 'intent market_info::handle_market_info(&env, &store, &verified, now).await;
        }

        // Resolve the dispatch config (risk caps, pip fallback, account caps)
        // at this edge so `run_action`/`run_enter` are `Env`-free. Built once,
        // shared by both broker arms (it's a function of the intent, not the
        // broker).
        let cfg = build_dispatch_config(&env, &verified).await;

        // Broker dispatch.
        let result = match verified.intent.broker {
            BrokerKind::Oanda => {
                match acquire_oanda_broker(&env, verified.intent.account.as_deref()).await {
                    Some(broker) => run_action(&broker, &store, &verified, &cfg, now, &yaml).await,
                    None => break 'intent Response::error("oanda login failed", 500),
                }
            }
            BrokerKind::TradeNation => {
                match acquire_tn_broker(&env, verified.intent.account.as_deref()).await {
                    Some(broker) => {
                        let adapter = TradeNationAdapter(broker);
                        run_action(&adapter, &store, &verified, &cfg, now, &yaml).await
                    }
                    None => {
                        break 'intent Response::error(
                            "tradenation login failed (missing account, bad credentials, or expired \
                         session — check worker logs)",
                            503,
                        );
                    }
                }
            }
        };

        // Every dispatch path records a seen entry — success, broker failure,
        // or pre-broker rejection — so the operator can read back what
        // happened to this id from the `status` snapshot.
        record_dispatcher_outcome(&store, &verified, now, &result).await;
        match result {
            ActionResult::Ok(_) => Response::ok("ok"),
            ActionResult::Failed(_) => Response::error("action failed", 502),
            ActionResult::Rejected { status, body, .. } => Response::error(body, status),
        }
    };

    // Record the request to R2 (fail-soft, async). On a transport error
    // there's nothing useful to record.
    let (status, outcome) = match &resp {
        Ok(r) => (r.status_code(), format!("status {}", r.status_code())),
        Err(_) => (0, "transport-error".to_string()),
    };
    let (intent_id, trade_id) = recording::ids_from_body(&yaml);
    recording::record_to_r2(
        &env,
        &ctx,
        recording::RequestRecord {
            ts: received_ts,
            request_id,
            method,
            path,
            headers,
            body: yaml,
            intent_id,
            trade_id,
            status,
            outcome,
            logs: recording::take_logs(),
        },
    );
    resp
}

/// Dispatch on `/admin/...` paths. Method + path together select the
/// right admin handler. Path parsing is deliberately concrete here
/// rather than a router crate — we have 4 routes and one path-segment
/// parameter (`<name>`), and the explicit `match` is easier to audit.
async fn route_admin(req: &mut Request, env: &Env, path: &str) -> Result<Response> {
    // POST /admin/accounts                      — add (body)
    // DELETE /admin/accounts/<name>             — remove
    // POST   /admin/accounts/<name>/test        — verify creds
    // POST   /admin/adopt-trade                 — adopt a broker-side trade
    let method = req.method();
    if method == Method::Post && path == "/admin/accounts" {
        return admin::handle_add(req, env).await;
    }
    if method == Method::Post && path == "/admin/adopt-trade" {
        return admin::handle_adopt(req, env).await;
    }
    if let Some(rest) = path.strip_prefix("/admin/accounts/")
        && !rest.is_empty()
    {
        if let Some(name) = rest.strip_suffix("/test")
            && method == Method::Post
            && !name.is_empty()
            && !name.contains('/')
        {
            return admin::handle_test(req, env, name).await;
        }
        if method == Method::Delete && !rest.contains('/') {
            return admin::handle_remove(req, env, rest).await;
        }
    }
    Response::error("not found", 404)
}

/// Map a worker-free [`ControlResult`] to a worker [`Response`] at the HTTP
/// edge. The control handlers are now worker-free (they return `ControlResult`);
/// this thin wrapper restores the `Result<Response>` the fetch loop yields,
/// staying byte-faithful to the pre-refactor responses (a `2xx` body → `ok`,
/// everything else → `error`).
fn control_to_response(c: ControlResult) -> Result<Response> {
    if c.is_success() {
        Response::ok(c.body)
    } else {
        Response::error(c.body, c.status)
    }
}

/// Handle the `plan-purge` action: delete **every** trace of a journaled trade.
///
/// A superset of [`handle_plan_delete`]. Beyond the `plan:` / `plan-state:` /
/// `archived-plan:` rows that delete drops, purge also clears the no-TTL
/// per-trade lifecycle rows — `entry-attempt:` (and the `order-body:` rows their
/// `broker_trade_id`/order ids point at), `control-event:` — plus the
/// enumerable trade-scoped control rows (`pause:` / `news:`), and deletes the
/// trade's R2 `ticks/` bundles. Idempotent: purging a trade with nothing left is
/// a no-op `ok`, not an error.
///
/// `veto:` / `prep:` rows are intentionally **not** swept here: they keep their
/// window TTL (expiry is their feature) and self-clear, and their lifecycle is
/// already captured durably in the `control-event:` trail this purge drops. So
/// purge removes the *audit + lifecycle* state, not the still-live control gates
/// (which a purge of a finished trade has none of anyway).
async fn handle_plan_purge<S: StateStore>(
    store: &S,
    env: &Env,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> ControlResult {
    let Some(target) = verified.intent.trade_id.as_deref() else {
        return ControlResult::error("plan-purge requires a `trade_id`", 400);
    };

    // 1) Drop the plan / state / archived rows across every scope (same scan as
    //    plan-delete). We re-list rather than call handle_plan_delete so we keep
    //    the per-scope account for the lifecycle-row clears below.
    let plans = store.list_all_trade_plans().await.unwrap_or_default();
    let archived = store.list_all_archived_plans().await.unwrap_or_default();
    let scopes: Vec<Option<String>> = plans
        .iter()
        .filter(|s| s.plan.trade_id == target)
        .map(|s| s.account.clone())
        .chain(
            archived
                .iter()
                .filter(|s| s.plan.trade_id == target)
                .map(|s| s.account.clone()),
        )
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    // If no plan/archive names this trade, still attempt a global-scope sweep of
    // the lifecycle rows (a trade can be purged after its plan rows already aged
    // out / were deleted).
    let scopes = if scopes.is_empty() {
        vec![None]
    } else {
        scopes
    };

    let mut cleared = 0usize;
    for scope in &scopes {
        let account = scope.as_deref();
        // entry-attempts → also drop each attempt's order-body, then the attempt.
        if let Ok(attempts) = store.list_entry_attempts(account, target).await {
            for a in &attempts {
                if let Some(oid) = &a.broker_trade_id
                    && store.delete_order_body(oid).await.is_ok()
                {
                    cleared += 1;
                }
                if store
                    .delete_entry_attempt(account, target, a.attempt_no)
                    .await
                    .is_ok()
                {
                    cleared += 1;
                }
            }
        }
        // pauses + news windows (trade-scoped, enumerable).
        if let Ok(pauses) = store.list_pauses_for_trade(target).await {
            for p in &pauses {
                if store.clear_pause(target, &p.blackout_id).await.is_ok() {
                    cleared += 1;
                }
            }
        }
        if let Ok(windows) = store.list_news_windows_for_trade(target).await {
            for w in &windows {
                if store.clear_news_window(target, &w.news_id).await.is_ok() {
                    cleared += 1;
                }
            }
        }
        // control-event audit trail.
        if store.clear_control_events(account, target).await.is_ok() {
            cleared += 1;
        }
        // plan / state / archived.
        store.clear_trade_plan(account, target).await.ok();
        store.clear_plan_state(account, target).await.ok();
        store.clear_archived_plan(account, target).await.ok();
    }

    // 2) R2 tick bundles for this trade.
    let r2_deleted = r2_purge::purge_trade_ticks(env, target)
        .await
        .unwrap_or_else(|err| {
            rlog_err!("plan-purge: R2 ticks purge failed for {target}: {err}");
            0
        });

    rlog!("plan-purge: trade_id={target} kv_cleared={cleared} r2_ticks_deleted={r2_deleted}");
    let outcome = format!("plan-purged: {target} (kv={cleared} r2={r2_deleted})");
    record_seen(store, verified, now, &outcome).await;
    ControlResult::ok(outcome)
}

/// Handle the `purge-older-than` action: bulk-delete R2 `req/` + `ticks/`
/// bundles whose date partition is strictly older than the cutoff carried in
/// `intent.not_before`. KV is untouched (per-trade KV rows are dropped by
/// `plan purge`). Manual retention housekeeping for the no-TTL recording bucket.
async fn handle_purge_older_than(
    env: &Env,
    verified: &incoming::Verified,
    _now: chrono::DateTime<chrono::Utc>,
) -> ControlResult {
    let Some(cutoff) = verified.intent.not_before else {
        return ControlResult::error("purge-older-than requires a cutoff in `not_before`", 400);
    };
    let deleted = match r2_purge::purge_older_than(env, cutoff).await {
        Ok(n) => n,
        Err(err) => {
            rlog_err!("purge-older-than: {err}");
            return ControlResult::error("r2 purge error", 500);
        }
    };
    rlog!("purge-older-than: cutoff={cutoff} r2_deleted={deleted}");
    ControlResult::ok(format!("purged-older-than: {cutoff} ({deleted})"))
}

/// Resolve an [`OandaBroker`] for the request.
///
/// - When `account` is `Some(name)`, looks up the account's metadata
///   in the KV index and uses its `oanda_account_id` as the sub-account
///   id. Falls back to the worker-global `OANDA_ACCOUNT_ID` secret if
///   the metadata exists but lacks an id (operator forgot to set it).
/// - When `account` is `None`, uses the worker-global secret directly
///   (legacy behaviour, preserved for intents that pre-date the
///   first-class account system).
///
/// In both cases the API token comes from the shared worker-wide
/// `OANDA_API_KEY` — OANDA only issues one token per user, and that
/// token can address every sub-account.
pub(crate) async fn acquire_oanda_broker(env: &Env, account: Option<&str>) -> Option<OandaBroker> {
    match account {
        Some(name) => acquire_oanda_broker_for_account(env, name).await,
        None => oanda_login(env).await,
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn acquire_oanda_broker_for_account(_env: &Env, _name: &str) -> Option<OandaBroker> {
    None
}

#[cfg(target_arch = "wasm32")]
async fn acquire_oanda_broker_for_account(env: &Env, name: &str) -> Option<OandaBroker> {
    use trade_control_core::account::MetadataStore;
    use trade_control_core::intent::BrokerKind;

    let kv = match env.kv(KV_NAMESPACE) {
        Ok(kv) => kv,
        Err(err) => {
            rlog_err!("oanda[{name}]: KV binding missing: {err:?}");
            return None;
        }
    };
    let metadata = accounts::KvMetadataStore::new(kv);
    let meta = match metadata.get(name).await {
        Ok(m) => m,
        Err(err) => {
            rlog_err!("oanda[{name}]: metadata lookup failed: {err}");
            return None;
        }
    };
    if meta.broker != BrokerKind::Oanda {
        rlog_err!(
            "oanda[{name}]: account broker={:?} but intent routed to oanda",
            meta.broker
        );
        return None;
    }
    // Practice vs live is per-account, derived from the account's `kind`
    // — not the worker-global `OANDA_LIVE` secret. A demo account always
    // hits the practice host, a live account the live host, in one worker.
    let live = meta.kind.is_live();
    match meta.oanda_account_id {
        Some(id) => oanda_login_with(env, id, live).await,
        None => {
            rlog_err!(
                "oanda[{name}]: metadata has no `oanda_account_id` — re-run `trade-control account add` \
                 to set it, or fall back via worker-global OANDA_ACCOUNT_ID by omitting `account:` \
                 from the intent"
            );
            oanda_login(env).await
        }
    }
}

/// Resolve a `TradeNationBroker`, refreshing the session as needed.
///
/// When `account` is `Some(name)`, the worker routes through the
/// first-class account store — looks the metadata + credentials up,
/// caches the session under `tn:session:<name>`, and uses the
/// account's own username / password to log in.
///
/// Returns `None` if the account is missing, the broker tag doesn't
/// match, credentials can't be resolved, or login fails — each failure
/// is logged at `rlog_err!`. Intents without `account:` are
/// rejected at the caller; the legacy shared-session paths are gone.
pub(crate) async fn acquire_tn_broker(
    env: &Env,
    account: Option<&str>,
) -> Option<broker_tradenation::TradeNationBroker> {
    match account {
        Some(name) => acquire_tn_broker_for_account(env, name).await,
        None => {
            rlog_err!(
                "tn: intent missing `account` — TradeNation routing requires a named account \
                 (use `trade-control account add <name>` to register one)"
            );
            None
        }
    }
}

/// Per-account KV cache slot for a session.
#[cfg(target_arch = "wasm32")]
fn tn_session_cache_key(account: &str) -> String {
    format!("tn:session:{account}")
}

/// Look up the per-account caps from the metadata store. Returns
/// `AccountCaps::default()` (all `None`) when the account isn't named
/// (OANDA path), when the KV binding is missing, or when the metadata
/// lookup fails — caps are advisory tighteners, so a missing record
/// just falls through to worker-wide defaults.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn load_account_caps(
    _env: &Env,
    _account: Option<&str>,
) -> trade_control_core::account::AccountCaps {
    trade_control_core::account::AccountCaps::default()
}

#[cfg(target_arch = "wasm32")]
pub(crate) async fn load_account_caps(
    env: &Env,
    account: Option<&str>,
) -> trade_control_core::account::AccountCaps {
    use trade_control_core::account::{AccountCaps, MetadataStore};

    let Some(name) = account else {
        return AccountCaps::default();
    };
    let kv = match env.kv(KV_NAMESPACE) {
        Ok(kv) => kv,
        Err(err) => {
            rlog_err!("caps[{name}]: KV binding missing: {err:?}");
            return AccountCaps::default();
        }
    };
    let metadata = accounts::KvMetadataStore::new(kv);
    match metadata.get(name).await {
        Ok(m) => m.caps,
        Err(err) => {
            rlog_err!("caps[{name}]: metadata lookup failed: {err}");
            AccountCaps::default()
        }
    }
}

/// Account-aware path. Looks up the metadata + credentials via the
/// account store, verifies the broker tag is TradeNation, then logs in
/// using the account's own username / password.
#[cfg(not(target_arch = "wasm32"))]
async fn acquire_tn_broker_for_account(
    _env: &Env,
    _name: &str,
) -> Option<broker_tradenation::TradeNationBroker> {
    // Native test builds — no `worker::Fetch`, so the redirect-chain
    // login can't run. The wasm cfg below is the production path.
    None
}

#[cfg(target_arch = "wasm32")]
async fn acquire_tn_broker_for_account(
    env: &Env,
    name: &str,
) -> Option<broker_tradenation::TradeNationBroker> {
    use trade_control_core::account::{
        Credentials, CredentialsResolver, MetadataStore, TradeNationKind,
    };
    use trade_control_core::intent::BrokerKind;

    let kv = match env.kv(KV_NAMESPACE) {
        Ok(kv) => kv,
        Err(err) => {
            rlog_err!("tn[{name}]: KV binding missing: {err:?}");
            return None;
        }
    };
    let metadata = accounts::KvMetadataStore::new(kv.clone());
    let meta = match metadata.get(name).await {
        Ok(m) => m,
        Err(err) => {
            rlog_err!("tn[{name}]: metadata lookup failed: {err}");
            return None;
        }
    };
    if meta.broker != BrokerKind::TradeNation {
        rlog_err!(
            "tn[{name}]: account broker={:?} but intent routed to tradenation",
            meta.broker
        );
        return None;
    }

    // 1. Try the per-account cached session.
    let cache_key = tn_session_cache_key(name);
    match kv.get(&cache_key).text().await {
        Ok(Some(cached)) => {
            if let Some(broker) = broker_tradenation::login(&cached).await {
                rlog!("tn[{name}]: using cached session");
                return Some(broker);
            }
            rlog!("tn[{name}]: cached session rejected, will re-login");
        }
        Ok(None) => rlog!("tn[{name}]: no cached session, will login"),
        Err(err) => rlog_err!("tn[{name}]: KV get session: {err:?}"),
    }

    // 2. Resolve credentials.
    let resolver = accounts::SecretCredentialsResolver::new(env, &metadata);
    let creds = match resolver.resolve(name).await {
        Ok(c) => c,
        Err(err) => {
            rlog_err!("tn[{name}]: credentials resolve: {err}");
            return None;
        }
    };
    let tn_creds = match creds {
        Credentials::TradeNation(c) => c,
        Credentials::Oanda(_) => {
            rlog_err!("tn[{name}]: credential payload is OANDA — broker mismatch");
            return None;
        }
    };

    // 3. Login per kind. Each path logs in, JSON-serialises the
    //    session into the per-account KV slot, then hands the JSON to
    //    `broker_tradenation::login` to build the live broker handle.
    match tn_creds.kind {
        TradeNationKind::Demo => {
            login_and_cache_demo(
                env,
                name,
                &cache_key,
                &tn_creds.username,
                &tn_creds.password,
            )
            .await
        }
        TradeNationKind::Live => {
            login_and_cache_live(
                env,
                name,
                &cache_key,
                &tn_creds.username,
                &tn_creds.password,
            )
            .await
        }
    }
}

#[cfg(target_arch = "wasm32")]
async fn login_and_cache_demo(
    env: &Env,
    account_name: &str,
    cache_key: &str,
    username: &str,
    password: &str,
) -> Option<broker_tradenation::TradeNationBroker> {
    let session = match tn_login::login_demo(username, password).await {
        Ok(s) => s,
        Err(err) => {
            rlog_err!("tn[{account_name}]: demo login failed: {err}");
            return None;
        }
    };
    cache_and_open(env, account_name, cache_key, "demo", &session).await
}

/// Live counterpart to `login_and_cache_demo`. The two have the same
/// cache-then-open tail; only the login function differs. Live login
/// is much slower than demo (3 JSON hops + redirect chain vs one
/// redirect chain) so the cache is even more important here.
#[cfg(target_arch = "wasm32")]
async fn login_and_cache_live(
    env: &Env,
    account_name: &str,
    cache_key: &str,
    username: &str,
    password: &str,
) -> Option<broker_tradenation::TradeNationBroker> {
    let session = match tn_login::login_live(username, password).await {
        Ok(s) => s,
        Err(err) => {
            rlog_err!("tn[{account_name}]: live login failed: {err}");
            return None;
        }
    };
    cache_and_open(env, account_name, cache_key, "live", &session).await
}

/// Serialise the freshly-minted `Session`, write it into the
/// per-account KV slot, then hand the JSON to
/// `broker_tradenation::login` to build the broker handle.
///
/// KV write failures are logged but don't abort — the operator still
/// gets a working broker for this request; the next request just pays
/// the login cost again.
#[cfg(target_arch = "wasm32")]
async fn cache_and_open(
    env: &Env,
    account_name: &str,
    cache_key: &str,
    kind_label: &'static str,
    session: &tradenation_api::Session,
) -> Option<broker_tradenation::TradeNationBroker> {
    let json = match serde_json::to_string(session) {
        Ok(s) => s,
        Err(err) => {
            rlog_err!("tn[{account_name}]: serialise session: {err}");
            return None;
        }
    };
    if let Ok(kv) = env.kv(KV_NAMESPACE) {
        match kv.put(cache_key, json.clone()) {
            Ok(builder) => {
                if let Err(err) = builder.execute().await {
                    rlog_err!("tn[{account_name}]: KV put session execute: {err:?}");
                }
            }
            Err(err) => rlog_err!("tn[{account_name}]: KV put session builder: {err:?}"),
        }
        write_session_meta(&kv, account_name).await;
    }
    if let Some(broker) = broker_tradenation::login(&json).await {
        rlog!("tn[{account_name}]: fresh {kind_label} login");
        return Some(broker);
    }
    rlog_err!("tn[{account_name}]: fresh session rejected by broker_tradenation::login");
    None
}

/// Best-effort write of the sibling `tn:session_meta:{account}` slot so
/// the cron pre-warm has a `cached_at` timestamp to compare against
/// `STALE_AFTER`. Failures are logged but never abort — the cron path
/// just treats a missing meta record as "stale" and re-logs in.
#[cfg(target_arch = "wasm32")]
async fn write_session_meta(kv: &worker::kv::KvStore, account_name: &str) {
    let meta = cron::session_meta::SessionMeta {
        cached_at: chrono::Utc::now(),
    };
    let json = match serde_json::to_string(&meta) {
        Ok(s) => s,
        Err(err) => {
            rlog_err!("tn[{account_name}]: serialise session_meta: {err}");
            return;
        }
    };
    let key = cron::session_meta::key(account_name);
    match kv.put(&key, json) {
        Ok(builder) => {
            if let Err(err) = builder.execute().await {
                rlog_err!("tn[{account_name}]: KV put session_meta execute: {err:?}");
            }
        }
        Err(err) => rlog_err!("tn[{account_name}]: KV put session_meta builder: {err:?}"),
    }
}

/// Read a secret. Returns `None` if the binding is absent or unreadable.
/// Silent on absence — callers decide whether a miss is an error worth logging.
pub(crate) fn get_secret(name: &str, env: &Env) -> Option<String> {
    env.secret(name).map(|value| value.to_string()).ok()
}

/// Read + parse the HMAC signing key the worker verifies incoming bodies with.
/// Same secret the fetch path reads at `src/lib.rs` top of `fetch`; factored
/// out so the spread-blackout crons can re-verify a stored signed body via
/// [`incoming::parse_and_verify`] (the only constructor of a [`incoming::Verified`]).
/// `None` (logged) when the secret is missing or not valid hex.
pub(crate) fn signing_key(env: &Env) -> Option<Vec<u8>> {
    let key_hex = match get_secret(SIGNING_KEY_SECRET, env) {
        Some(s) => s,
        None => {
            rlog_err!("missing required secret: {SIGNING_KEY_SECRET}");
            return None;
        }
    };
    match sig::parse_key_hex(&key_hex) {
        Ok(k) => Some(k.to_vec()),
        Err(err) => {
            rlog_err!("SIGNING_KEY is not valid hex: {err}");
            None
        }
    }
}

/// Read a numeric secret, falling back to `default` if missing or unparsable.
fn secret_or_default(env: &Env, name: &str, default: f64) -> f64 {
    get_secret(name, env)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Pip size for an instrument. Override via `PIP_SIZE_<INSTRUMENT>` secret
/// (e.g. `PIP_SIZE_USD_JPY=0.01`). Indices like SPX500_USD also need overrides.
/// Fallback pip size when the signed intent carries no baked `pip_size`
/// (steps 2–3 of the precedence in `run_enter`): the per-instrument
/// `PIP_SIZE_<instrument>` secret, then the forex `DEFAULT_PIP_SIZE`. The
/// baked `intent.pip_size` is the primary source and is preferred at the
/// call site — this is only reached for intents armed before pip-baking
/// landed, or armed outside `tv-arm`.
fn pip_size_for(env: &Env, instrument: &str) -> f64 {
    let key = format!("{PIP_SIZE_SECRET_PREFIX}{instrument}");
    get_secret(&key, env)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PIP_SIZE)
}

/// Resolve the [`DispatchConfig`](trade_control_core::dispatch_config::DispatchConfig)
/// for an enter, off the Cloudflare `Env`. This is the wasm worker's "edge"
/// resolver — it reads the worker-wide risk caps, the per-instrument pip-size
/// fallback, and the per-account caps (async, from the KV account index) up
/// front so `run_enter` itself is `Env`-free. The native runtime has its own
/// edge resolver built from `Secrets` + the Postgres account index.
///
/// `pip_size` here is only the *fallback* (the per-instrument secret →
/// `DEFAULT_PIP_SIZE`); `run_enter` still prefers the intent's baked `pip_size`
/// over it. Caps are looked up per the intent's `account` (default for an
/// unnamed account).
pub(crate) async fn build_dispatch_config(
    env: &Env,
    verified: &incoming::Verified,
) -> trade_control_core::dispatch_config::DispatchConfig {
    let caps = load_account_caps(env, verified.intent.account.as_deref()).await;
    trade_control_core::dispatch_config::DispatchConfig {
        worker_max_risk_pct: secret_or_default(env, MAX_RISK_PCT_PER_TRADE_SECRET, 1.0),
        worker_max_open_positions: secret_or_default(env, MAX_OPEN_POSITIONS_SECRET, 3.0) as u32,
        pip_size: pip_size_for(env, &verified.intent.instrument),
        caps,
    }
}
