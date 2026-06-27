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
mod allow_close_gate;
mod allow_entry_gate;
mod candle_gate;
mod cron;
mod diag;
mod market_blackout;
mod market_info;
mod r2_purge;
mod recover_entry;
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
use serde::Serialize;
use trade_control_core::broker::{Broker, EntryError, EntryRequest};
use trade_control_core::incoming::{self, IncomingDisposition, parse_and_verify};
use trade_control_core::intent::{
    Action, BrokerKind, Intent, MW_CANCEL_VETO_NAME, MwAnchors, MwUpdate, REVERSAL_VETO_NAME,
    ResolveError, Resolved, Shell, VetoLevel, effective_mw_params, is_inside_any, plan_mw_update,
};
use trade_control_core::rules::{self, RuleError};
use trade_control_core::sig;
use trade_control_core::state::{
    StateError, StateStore, clear_named_preps, clear_named_vetos, veto_ttl_seconds,
};
use trade_control_core::tunable::Tunable;

/// Resolve a [`Tunable<u32>`] against Phase 1 scope only (shell
/// anchors). Used by the `Invalidate`, `Prep`, and `Veto` action
/// paths — none of which builds a `Resolved`, so derived geometry
/// bindings aren't available. `default` is the fallback when the
/// field is absent. On script error returns a telemetry string the
/// caller wraps into an `ActionResult::Rejected`.
fn resolve_phase1_u32(
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

/// Response body for the `unlock` action. Serialised as YAML.
#[derive(Serialize)]
struct UnlockResponse {
    unlocked: String,
    was_cooled_down: bool,
}

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
            Action::Status => break 'intent handle_status(&store, &verified, now).await,
            Action::Unlock => break 'intent handle_unlock(&store, &verified, now).await,
            Action::Prep => break 'intent handle_prep(&store, &verified, now).await,
            Action::PrepExpire => break 'intent handle_prep_expire(&store, &verified, now).await,
            Action::Veto => {
                if matches!(
                    verified.intent.level.unwrap_or_default(),
                    VetoLevel::StopNextEntry
                ) {
                    break 'intent handle_veto(&store, &verified, now).await;
                }
                // Higher-level vetos need the broker; fall through.
            }
            Action::ClearPrep => break 'intent handle_clear_prep(&store, &verified, now).await,
            Action::ClearVeto => break 'intent handle_clear_veto(&store, &verified, now).await,
            Action::Pause => break 'intent handle_pause(&store, &verified, now).await,
            Action::Resume => break 'intent handle_resume(&store, &verified, now).await,
            Action::NewsStart => break 'intent handle_news_start(&store, &verified, now).await,
            Action::NewsEnd => break 'intent handle_news_end(&store, &verified, now).await,
            Action::Register => break 'intent handle_register(&store, &verified, now).await,
            Action::PlanList => break 'intent handle_plan_list(&store, &verified, now).await,
            Action::PlanShow => break 'intent handle_plan_show(&store, &verified, now).await,
            Action::PlanDelete => break 'intent handle_plan_delete(&store, &verified, now).await,
            // PlanPurge / PurgeOlderThan touch R2 (the `env`), so — like
            // MarketInfo — they're dispatched here with `env` in scope rather
            // than through the broker-less `run_action`.
            Action::PlanPurge => {
                break 'intent handle_plan_purge(&store, &env, &verified, now).await;
            }
            Action::PurgeOlderThan => {
                break 'intent handle_purge_older_than(&env, &verified, now).await;
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

        // Broker dispatch.
        let result = match verified.intent.broker {
            BrokerKind::Oanda => {
                match acquire_oanda_broker(&env, verified.intent.account.as_deref()).await {
                    Some(broker) => run_action(&broker, &store, &verified, &env, now, &yaml).await,
                    None => break 'intent Response::error("oanda login failed", 500),
                }
            }
            BrokerKind::TradeNation => {
                match acquire_tn_broker(&env, verified.intent.account.as_deref()).await {
                    Some(broker) => {
                        let adapter = TradeNationAdapter(broker);
                        run_action(&adapter, &store, &verified, &env, now, &yaml).await
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
            ActionResult::Rejected { response, .. } => response,
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

/// Record the dispatcher's outcome on the seen-by-id index.
///
/// **Only `Ok` writes.** `Failed` (502 from a broker call) and
/// `Rejected` (any gate or pre-broker reason) are logged via
/// `rlog!` for post-mortem visibility but deliberately do not
/// consume the intent id. The next fire of the same alert is allowed
/// through.
///
/// Rationale (CHF/JPY 2026-06-02 incident). Earlier worker versions
/// wrote `mark_seen` for every variant, which poisoned the id for the
/// rest of the alert's `not_after` window. A real instance: an
/// `enter` alert fired 6 times in 9h. Fire 4 was correctly rejected
/// with `rejected: missing-prep (break-and-close)` — the prep had
/// not been set yet, but it *could* have been later in the window.
/// That rejection poisoned the id, so fires 5 (a confirmed signal,
/// the entry the operator actually wanted) and 6 both 409'd on
/// `is_seen` before reaching the `allow_entry` script gate. Every
/// non-`Ok` outcome is either transient (gate condition might flip)
/// or terminal-but-idempotent (parse error, `resolve-failed`,
/// `retry-cap` — next fire will reject the same way). Letting them
/// refire is harmless KV churn; poisoning them silently breaks
/// within-window legitimate fires.
///
/// Control actions (`prep`, level-1 `veto`, `pause`, `clear-*`, etc.)
/// use a separate [`record_seen`] helper and *do* mark seen on
/// completion — that's legitimate idempotency for state-set
/// operations (a `prep` message replayed twice shouldn't refresh its
/// TTL twice).
///
/// Generic over [`StateStore`] so native (non-wasm) tests can pass a
/// `MemStateStore`-style fake — `worker::Response` construction stays
/// in the caller, keeping this function callable off-wasm.
async fn record_dispatcher_outcome<S: StateStore>(
    store: &S,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
    result: &ActionResult,
) {
    match seen_decision(result) {
        SeenDecision::Mark { outcome } => {
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
                rlog_err!("KV mark_seen after action: {err}");
            }
        }
        SeenDecision::Skip { kind, outcome } => {
            log_skip(kind, &verified.intent.id, outcome);
        }
    }
}

/// Log a skipped (non-`Ok`) dispatcher outcome. `rlog!` is native-safe
/// (off-wasm it routes through `tracing`) and buffers the line into the
/// per-request R2 record.
fn log_skip(kind: &str, id: &str, outcome: &str) {
    rlog!("entry-path {kind} (no mark_seen): id={id} outcome={outcome}");
}

/// True when this intent is a multi-shot `enter` — an `enter` that
/// opted into `max_retries` (anything other than the default
/// `Static(0)`) and carries a `trade_id`.
///
/// For these the top-level intent-id replay guard in [`fetch`] must
/// **not** 409: the alert bakes one static intent id and re-fires it on
/// every signal bar, so the first accepted fire would otherwise poison
/// the id and block every legitimate re-entry. The real replay
/// authority for multi-shot is `retry_gate::evaluate` (run from
/// `run_enter`), which dedups true same-bar re-fires on `shell.time`
/// and rejects 412 when a prior attempt is still open.
///
/// The `trade_id.is_some()` clause is load-bearing: without a
/// `trade_id` the retry gate does no per-bar dedup (see
/// `retry_gate::evaluate`), so such an intent must stay on the
/// top-level 409 path. Single-shot enters and every control action
/// return `false` and keep the byte-identical top-level 409.
///
/// Pure (`&Intent -> bool`) so native unit tests can exercise the rule
/// without building a `worker::Response` — same rationale as
/// [`seen_decision`].
fn is_multishot_enter(intent: &trade_control_core::intent::Intent) -> bool {
    matches!(intent.action, Action::Enter)
        && !matches!(intent.max_retries, Tunable::Static(0))
        && intent.trade_id.is_some()
}

/// Pure helper: classify an [`ActionResult`] into "write to seen" vs
/// "log only". Pulled out so native unit tests can exercise the rule
/// without constructing a `worker::Response` (which calls into
/// wasm-bindgen at construction time and panics off-wasm).
fn seen_decision(result: &ActionResult) -> SeenDecision<'_> {
    match result {
        ActionResult::Ok(outcome) => SeenDecision::Mark { outcome },
        ActionResult::Failed(outcome) => SeenDecision::Skip {
            kind: "failed",
            outcome,
        },
        ActionResult::Rejected { outcome, .. } => SeenDecision::Skip {
            kind: "rejected",
            outcome,
        },
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SeenDecision<'a> {
    Mark {
        outcome: &'a str,
    },
    Skip {
        kind: &'static str,
        outcome: &'a str,
    },
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

/// Outcome of an action dispatch. Every variant carries a short
/// human-readable `outcome` string.
///
/// Only [`ActionResult::Ok`] lands in the seen-by-id index — see
/// [`record_dispatcher_outcome`] for why. `Failed` and `Rejected`
/// outcomes are logged via `rlog!` for post-mortem visibility
/// but do not consume the intent id.
pub(crate) enum ActionResult {
    /// Action completed successfully. The outcome (e.g. `"entered"`)
    /// is recorded against the seen id so a replay of the same alert
    /// body 409s instead of placing a duplicate order.
    Ok(String),
    /// Action reached the broker but the broker call failed. HTTP
    /// response is 502. **Not** recorded against the seen id — the
    /// next fire is allowed to retry.
    Failed(String),
    /// Action was rejected before reaching the broker (gate, validation,
    /// state error). The `response` is returned to the caller. **Not**
    /// recorded against the seen id — gate rejections are transient
    /// (the condition might flip later in the alert window), so the
    /// next fire is allowed through.
    Rejected {
        response: Result<Response>,
        outcome: String,
    },
}

impl ActionResult {
    /// Short, `Response`-free description for logging (the `Rejected` variant
    /// holds a `worker::Response`, which isn't `Debug`). Used by the
    /// spread-blackout restore re-drive, which logs the outcome but does not
    /// route it through the HTTP dispatcher.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Ok(s) => format!("Ok({s})"),
            Self::Failed(s) => format!("Failed({s})"),
            Self::Rejected { outcome, .. } => format!("Rejected({outcome})"),
        }
    }
}

/// Dispatch `Enter` / `Close` / `Invalidate` / escalated `Veto` against an
/// authenticated broker. Status / Unlock / Prep / `stop-next-entry` Veto /
/// Clear-* are handled before this function and never reach it.
async fn run_action<B: Broker>(
    broker: &B,
    store: &KvStateStore,
    verified: &incoming::Verified,
    env: &Env,
    now: chrono::DateTime<chrono::Utc>,
    raw_body: &str,
) -> ActionResult {
    match verified.intent.action {
        Action::Enter => run_enter(broker, store, verified, env, now, Some(raw_body), None).await,
        Action::Close => run_close(broker, store, verified, now).await,
        Action::Invalidate => run_invalidate(broker, store, verified, now).await,
        Action::Veto => run_veto_with_broker(broker, store, verified, now).await,
        Action::Status
        | Action::Unlock
        | Action::Prep
        | Action::PrepExpire
        | Action::ClearPrep
        | Action::ClearVeto
        | Action::Pause
        | Action::Resume
        | Action::NewsStart
        | Action::NewsEnd
        | Action::Register
        | Action::PlanList
        | Action::PlanShow
        | Action::PlanDelete
        | Action::PlanPurge
        | Action::PurgeOlderThan
        // MarketInfo needs the concrete TradeNation broker (its `market_info`
        // is not on the generic `Broker` trait), so it's dispatched in the
        // broker-acquire section before this generic function — never here.
        | Action::MarketInfo => {
            // Handled before broker dispatch; never reached here.
            unreachable!("non-broker actions handled before broker dispatch")
        }
    }
}

/// Dispatch an `Invalidate` intent: set an instrument cooldown and cancel any
/// pending orders for it. Extracted from [`run_action`] so the cron engine can
/// dispatch a fired invalidation veto through the identical path. `pub(crate)`
/// for that reuse.
pub(crate) async fn run_invalidate<B: Broker>(
    broker: &B,
    store: &KvStateStore,
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
                response: Response::error("cooldown_hours script error", 412),
                outcome,
            };
        }
    };
    let account = verified.intent.account.as_deref();
    if let Err(err) = store
        .set_cooldown(account, &verified.intent.instrument, hours, now)
        .await
    {
        rlog_err!("KV set_cooldown: {err}");
        return ActionResult::Rejected {
            response: Response::error("state error", 500),
            outcome: "rejected: state-error".into(),
        };
    }
    record_control_event_for(
        store,
        account,
        verified.intent.trade_id.as_deref(),
        trade_control_core::control_event::ControlKind::Cooldown,
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
    rlog!(
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

/// Dispatch a `Close` intent. The close reaches the broker only when
/// **every** layer of gating agrees:
///
/// 1. **Contextual window** (OR-composed) — the close is "at a real
///    reversal point". Up to two windows may be listed; *at least one*
///    must pass.
///      - News window — an active `news:<trade_id>:<news_id>` pair.
///      - Price window — broker's current price sits inside one of
///        `sr_bands`.
///
///    The new wire form is `inside_window: [news, price]` +
///    `sr_bands: [[lo, hi]]`. The deprecated form
///    (`require_news_window` + `require_price_in_ranges`) is still
///    accepted; validation guarantees an intent only carries one form.
/// 2. **Candle quality** (AND-composed) — `needs_golden` and
///    `needs_confirmed` shell-checks. Promoted to typed fields so the
///    consolidated reversal close can require a golden / confirmed
///    candle without dropping into Rhai.
/// 3. **`allow_close` script** (AND-composed) — operator's Tunable<bool>
///    sees the shell-anchor scope (same scope `allow_entry` sees, minus
///    derived geometry — closes don't compute SL/TP).
///
/// Both contextual gates are evaluated even after one passes, so the
/// log line records the full state and the outcome string can name
/// every failed gate when none passes.
async fn run_close<B: Broker>(
    broker: &B,
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> ActionResult {
    // News window. Old form: `require_news_window: Some(true)`. New
    // form: `inside_window` contains `News`. Mutual exclusion is
    // enforced at validate time, so at most one branch fires.
    let want_news = verified.intent.require_news_window == Some(true)
        || verified
            .intent
            .inside_window
            .contains(&trade_control_core::intent::EventWindow::News);
    let news_outcome = if want_news {
        let Some(tid) = verified.intent.trade_id.as_deref() else {
            return ActionResult::Rejected {
                response: Response::error("close with news-window gate requires `trade_id`", 400),
                outcome: "rejected: missing-trade-id".into(),
            };
        };
        match store.list_news_windows_for_trade(tid).await {
            Ok(windows) if windows.is_empty() => GateOutcome::Failed("no-news-window"),
            Ok(windows) => {
                let names: Vec<String> = windows
                    .iter()
                    .map(|w| match &w.reason {
                        Some(r) => format!("{}({r})", w.news_id),
                        None => w.news_id.clone(),
                    })
                    .collect();
                rlog!(
                    "close news-window gate passed: trade {tid} active=[{}]",
                    names.join(", ")
                );
                GateOutcome::Passed
            }
            Err(err) => {
                rlog_err!("KV list_news_windows_for_trade: {err}");
                return ActionResult::Rejected {
                    response: Response::error("state error", 500),
                    outcome: "rejected: state-error".into(),
                };
            }
        }
    } else {
        GateOutcome::NotSet
    };
    // Price window. Old form: `require_price_in_ranges: Some(ranges)`.
    // New form: `inside_window` contains `Price` (with bands in
    // `sr_bands`). Validation guarantees `sr_bands` is non-empty
    // exactly when `inside_window` lists Price.
    let price_ranges: Option<&[[f64; 2]]> = match verified.intent.require_price_in_ranges.as_deref()
    {
        Some(ranges) => Some(ranges),
        None if verified
            .intent
            .inside_window
            .contains(&trade_control_core::intent::EventWindow::Price) =>
        {
            Some(verified.intent.sr_bands.as_slice())
        }
        None => None,
    };
    let price_outcome = match price_ranges {
        Some(ranges) => match broker.get_current_price(&verified.intent.instrument).await {
            Ok(price) => match price_band_hit(price, ranges) {
                Some([lo, hi]) => {
                    rlog!(
                        "close price-range gate passed: {} price={price} in [{lo}, {hi}]",
                        verified.intent.instrument
                    );
                    GateOutcome::Passed
                }
                None => {
                    rlog!(
                        "close price-range gate failed: {} price={price} outside all bands {ranges:?}",
                        verified.intent.instrument
                    );
                    GateOutcome::Failed("price-out-of-range")
                }
            },
            Err(err) => {
                rlog_err!(
                    "broker get_current_price for {}: {err:?}",
                    verified.intent.instrument
                );
                return ActionResult::Rejected {
                    response: Response::error("price-fetch failed", 500),
                    outcome: "rejected: price-fetch-failed".into(),
                };
            }
        },
        None => GateOutcome::NotSet,
    };
    // Contextual gate (OR-composed).
    if let GateDecision::Reject { reason_code } = evaluate_close_gates(news_outcome, price_outcome)
    {
        return ActionResult::Rejected {
            response: Response::error("close gates not satisfied", 423),
            outcome: format!("rejected: {reason_code}"),
        };
    }
    // Candle quality + allow_close script (AND-composed with the
    // contextual gate). Pulled into `allow_close_gate::evaluate` so the
    // gate-mapping logic lives next to the entry-side analogue.
    match allow_close_gate::evaluate(&verified.intent, &verified.shell) {
        allow_close_gate::AllowCloseOutcome::Proceed => {}
        allow_close_gate::AllowCloseOutcome::Blocked => {
            rlog!(
                "close rejected: allow_close returned false (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                response: Response::error("close blocked", 412),
                outcome: "rejected: allow-close-false".into(),
            };
        }
        allow_close_gate::AllowCloseOutcome::NeedsGoldenUnmet => {
            rlog!(
                "close rejected: needs_golden set but shell.golden != Some(true) (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                response: Response::error("close blocked: needs-golden", 412),
                outcome: "rejected: needs-golden".into(),
            };
        }
        allow_close_gate::AllowCloseOutcome::NeedsConfirmedUnmet => {
            rlog!(
                "close rejected: needs_confirmed set but shell.signal_confirmed != Some(true) (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                response: Response::error("close blocked: needs-confirmed", 412),
                outcome: "rejected: needs-confirmed".into(),
            };
        }
        allow_close_gate::AllowCloseOutcome::ScriptError { kind, message } => {
            rlog_err!(
                "allow_close script error (id={}): {message}",
                verified.intent.id
            );
            return ActionResult::Rejected {
                response: Response::error("close blocked: script error", 412),
                outcome: format!("rejected: allow-close-{kind}"),
            };
        }
    }
    let ok = broker.close_positions(&verified.intent.instrument).await;
    // veto_on_reversal (experimental, opt-in): a reversal-close whose
    // gate passed is a real reversal signal. If the operator armed this
    // flag, also record a `reversal` veto for this trade_id so a *later*
    // enter is blocked — the case where the reversal lands before entry
    // and `close_positions` was a no-op. Written on every gate-pass
    // (idempotent key, TTL refreshed); independent of whether a position
    // was actually open. The close result itself still drives the
    // response below. Validation guarantees veto_on_reversal implies a
    // price window and a Close action; a missing trade_id is the only
    // remaining reason we'd skip — log it rather than fail the close.
    if verified.intent.veto_on_reversal {
        write_reversal_veto(store, verified, now).await;
    }
    if ok {
        ActionResult::Ok("closed".into())
    } else {
        ActionResult::Failed("close-failed".into())
    }
}

/// The veto a gate-passed reversal-close should write under the
/// `veto_on_reversal` hook. Borrowed from the intent so the KV call is a
/// thin wrapper; `None` means there's no `trade_id` to scope the veto to
/// (we log + skip rather than write a global veto). Pulled out of the
/// KV-calling path so the decision is unit-testable without a KV fixture.
struct ReversalVetoPlan<'a> {
    account: Option<&'a str>,
    trade_id: &'a str,
    instrument: &'a str,
    ttl_seconds: u64,
}

/// Decide the reversal veto for a gate-passed reversal-close. Returns
/// `None` when the intent carries no `trade_id` (vetos are trade-scoped;
/// a global reversal veto would bleed across setups). TTL follows the
/// same rule as a `too-high` veto — live for the life of the alert
/// window (`not_after` tail), with a zero `ttl_hours` component since a
/// reversal-close fires mid-window.
fn reversal_veto_plan<'a>(
    intent: &'a Intent,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<ReversalVetoPlan<'a>> {
    let trade_id = intent.trade_id.as_deref()?;
    Some(ReversalVetoPlan {
        account: intent.account.as_deref(),
        trade_id,
        instrument: &intent.instrument,
        ttl_seconds: veto_ttl_seconds(0, intent.not_after, now),
    })
}

/// Write the experimental `reversal` veto for a gate-passed
/// reversal-close. Best-effort: a KV failure or a missing `trade_id` is
/// logged and swallowed — the close has already happened and the veto is
/// an additive guard, not a precondition for it.
async fn write_reversal_veto(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) {
    let Some(plan) = reversal_veto_plan(&verified.intent, now) else {
        rlog!(
            "veto_on_reversal set but close has no trade_id (id={}); skipping reversal veto",
            verified.intent.id
        );
        return;
    };
    if let Err(err) = store
        .set_veto(
            plan.account,
            plan.trade_id,
            plan.instrument,
            REVERSAL_VETO_NAME,
            plan.ttl_seconds,
        )
        .await
    {
        rlog_err!("KV set_veto (reversal): {err}");
        return;
    }
    record_control_event_for(
        store,
        plan.account,
        Some(plan.trade_id),
        trade_control_core::control_event::ControlKind::Veto,
        REVERSAL_VETO_NAME,
        plan.instrument,
        plan.ttl_seconds,
        now,
        None,
    )
    .await;
    rlog!(
        "reversal veto set: instrument={} account={} trade_id={} name={REVERSAL_VETO_NAME} ttl={}s",
        plan.instrument,
        plan.account.unwrap_or("<global>"),
        plan.trade_id,
        plan.ttl_seconds,
    );
}

/// Per-gate evaluation result. `Failed` carries a short reason code
/// that lands in the outcome string when *no* set gate passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateOutcome {
    /// Gate was not configured on this intent.
    NotSet,
    /// Gate was configured and its condition was met.
    Passed,
    /// Gate was configured and its condition was not met.
    Failed(&'static str),
}

/// OR-composed gate decision. Used by [`run_close`] and unit tests.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GateDecision {
    /// At least one set gate passed (or no gates were set).
    Pass,
    /// One or more gates were set and all of them failed. The
    /// `reason_code` joins each failing gate's short code with `|`
    /// so an operator reading the seen index sees what was tried.
    Reject { reason_code: String },
}

/// Compose per-gate outcomes into a single Pass/Reject decision using
/// **OR** semantics: pass when no gates are set, pass when at least
/// one set gate passed, reject only when every set gate failed.
fn evaluate_close_gates(news: GateOutcome, price: GateOutcome) -> GateDecision {
    let outcomes = [news, price];
    let any_passed = outcomes.iter().any(|o| matches!(o, GateOutcome::Passed));
    let any_set = outcomes.iter().any(|o| !matches!(o, GateOutcome::NotSet));
    if any_passed || !any_set {
        return GateDecision::Pass;
    }
    let reason_code = outcomes
        .iter()
        .filter_map(|o| match o {
            GateOutcome::Failed(code) => Some(*code),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("|");
    GateDecision::Reject { reason_code }
}

/// Find the first `[lo, hi]` band in `ranges` that contains `price`
/// (inclusive on both ends). Returns `None` when `price` sits outside
/// every band. Pulled out of [`run_close`] so the gate logic itself
/// can be unit-tested without standing up a full broker + KV fixture.
fn price_band_hit(price: f64, ranges: &[[f64; 2]]) -> Option<[f64; 2]> {
    ranges
        .iter()
        .copied()
        .find(|[lo, hi]| price >= *lo && price <= *hi)
}

#[cfg(test)]
mod price_band_tests {
    use super::price_band_hit;

    #[test]
    fn price_inside_single_band_hits() {
        let ranges = [[1.0950, 1.0970]];
        assert_eq!(price_band_hit(1.0960, &ranges), Some([1.0950, 1.0970]));
    }

    #[test]
    fn price_on_band_endpoints_hits() {
        let ranges = [[1.0950, 1.0970]];
        assert!(price_band_hit(1.0950, &ranges).is_some());
        assert!(price_band_hit(1.0970, &ranges).is_some());
    }

    #[test]
    fn price_outside_all_bands_misses() {
        let ranges = [[1.0950, 1.0970], [1.1000, 1.1020]];
        assert_eq!(price_band_hit(1.0980, &ranges), None);
        assert_eq!(price_band_hit(1.0900, &ranges), None);
        assert_eq!(price_band_hit(1.1100, &ranges), None);
    }

    #[test]
    fn price_picks_first_matching_band_when_multiple_overlap() {
        let ranges = [[1.0950, 1.0970], [1.0960, 1.0980]];
        assert_eq!(price_band_hit(1.0965, &ranges), Some([1.0950, 1.0970]));
    }

    #[test]
    fn empty_ranges_always_misses() {
        assert_eq!(price_band_hit(1.0, &[]), None);
    }
}

#[cfg(test)]
mod close_gate_tests {
    use super::{GateDecision, GateOutcome, evaluate_close_gates};

    #[test]
    fn no_gates_set_passes() {
        assert_eq!(
            evaluate_close_gates(GateOutcome::NotSet, GateOutcome::NotSet),
            GateDecision::Pass,
        );
    }

    #[test]
    fn single_news_gate_passing_passes() {
        assert_eq!(
            evaluate_close_gates(GateOutcome::Passed, GateOutcome::NotSet),
            GateDecision::Pass,
        );
    }

    #[test]
    fn single_news_gate_failing_rejects_with_its_code() {
        assert_eq!(
            evaluate_close_gates(GateOutcome::Failed("no-news-window"), GateOutcome::NotSet),
            GateDecision::Reject {
                reason_code: "no-news-window".into(),
            },
        );
    }

    #[test]
    fn single_price_gate_failing_rejects_with_its_code() {
        assert_eq!(
            evaluate_close_gates(
                GateOutcome::NotSet,
                GateOutcome::Failed("price-out-of-range"),
            ),
            GateDecision::Reject {
                reason_code: "price-out-of-range".into(),
            },
        );
    }

    #[test]
    fn both_gates_set_news_passes_price_fails_passes() {
        assert_eq!(
            evaluate_close_gates(
                GateOutcome::Passed,
                GateOutcome::Failed("price-out-of-range"),
            ),
            GateDecision::Pass,
        );
    }

    #[test]
    fn both_gates_set_news_fails_price_passes_passes() {
        assert_eq!(
            evaluate_close_gates(GateOutcome::Failed("no-news-window"), GateOutcome::Passed),
            GateDecision::Pass,
        );
    }

    #[test]
    fn both_gates_set_both_pass_passes() {
        assert_eq!(
            evaluate_close_gates(GateOutcome::Passed, GateOutcome::Passed),
            GateDecision::Pass,
        );
    }

    #[test]
    fn both_gates_set_both_fail_rejects_with_joined_codes() {
        assert_eq!(
            evaluate_close_gates(
                GateOutcome::Failed("no-news-window"),
                GateOutcome::Failed("price-out-of-range"),
            ),
            GateDecision::Reject {
                reason_code: "no-news-window|price-out-of-range".into(),
            },
        );
    }
}

#[cfg(test)]
mod reversal_veto_tests {
    use super::{REVERSAL_VETO_NAME, reversal_veto_plan};
    use trade_control_core::intent::Intent;

    fn close_intent(yaml_extra: &str) -> Intent {
        let yaml = format!(
            "
            v: 1
            id: rev-close
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            inside_window: [price]
            sr_bands: [[1.0950, 1.0970]]
            veto_on_reversal: true
{yaml_extra}
        "
        );
        serde_yaml::from_str(&yaml).expect("close intent parses")
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        "2026-05-13T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn plan_is_none_without_trade_id() {
        // No trade_id → no trade-scoped veto to write (we don't fall back
        // to a global reversal veto that would bleed across setups).
        let intent = close_intent("");
        assert!(reversal_veto_plan(&intent, now()).is_none());
    }

    #[test]
    fn plan_scopes_to_trade_id_and_account() {
        let intent =
            close_intent("            trade_id: eurusd-hs-1\n            account: reversals");
        let plan = reversal_veto_plan(&intent, now()).expect("plan present");
        assert_eq!(plan.trade_id, "eurusd-hs-1");
        assert_eq!(plan.account, Some("reversals"));
        assert_eq!(plan.instrument, "EUR_USD");
    }

    #[test]
    fn plan_account_is_none_when_unset() {
        let intent = close_intent("            trade_id: eurusd-hs-1");
        let plan = reversal_veto_plan(&intent, now()).expect("plan present");
        assert_eq!(plan.account, None);
    }

    #[test]
    fn plan_ttl_lives_to_window_end() {
        // not_after is 2026-05-13T20:00:00Z, now is 12:00:00Z → 8h window.
        // veto_ttl_seconds(0, ..) = ttl_hours(0) + remaining(8h) = 8h, so
        // the reversal veto lives exactly to the end of the alert window —
        // killing this setup's remaining entries, no longer.
        let intent = close_intent("            trade_id: eurusd-hs-1");
        let plan = reversal_veto_plan(&intent, now()).expect("plan present");
        assert_eq!(plan.ttl_seconds, 8 * 3600);
    }

    #[test]
    fn veto_name_is_reversal() {
        assert_eq!(REVERSAL_VETO_NAME, "reversal");
    }
}

/// Run an `enter` intent end-to-end (gates → sizing → broker placement →
/// `recover_entry` fallback).
///
/// `raw_body` is the **exact signed YAML bytes** this intent arrived as, when
/// known. On a successful real placement we persist it under an
/// `order:{broker_order_id}` KV row so the spread-blackout apply cron can
/// recover it (it finds a broker *pending order*, not a signed intent) and
/// re-drive this same entry on recovery. `None` is passed only where no signed
/// body is available (there is none today — both the HTTP path and the
/// blackout re-drive supply it); a `None` simply skips the order-body write, so
/// such an order can't be blackout-cancelled-and-restored.
pub(crate) async fn run_enter<B: Broker>(
    broker: &B,
    store: &KvStateStore,
    verified: &incoming::Verified,
    env: &Env,
    now: chrono::DateTime<chrono::Utc>,
    raw_body: Option<&str>,
    // The trade's timeframe, when this enter was dispatched from a registered
    // plan (the engine path passes `Some(plan.granularity)`). The break-even
    // position cron needs it to fetch the right closed candles. The webhook and
    // blackout-restore re-drive paths have no plan timeframe in hand and pass
    // `None` — those enters simply don't get cron-managed break-even (the
    // signed enter still carries its `breakeven` rule; only the cron snapshot
    // is skipped without a granularity to fetch on).
    enter_granularity: Option<trade_control_core::broker::Granularity>,
) -> ActionResult {
    // Blackout gate — if any pause for this trade_id is active, reject
    // before doing any other work. Pauses are intentionally cheap to
    // check (one prefix list on the trade's own keys) so they can sit
    // ahead of the retry/cooldown/prep/veto chain. Trades minted
    // without a `trade_id` (legacy single-shot entries) bypass this
    // gate entirely — there's no key to look pauses up by.
    if let Some(tid) = verified.intent.trade_id.as_deref() {
        match store.list_pauses_for_trade(tid).await {
            Ok(pauses) if !pauses.is_empty() => {
                let blackouts: Vec<String> = pauses
                    .iter()
                    .map(|p| match &p.reason {
                        Some(r) => format!("{}({r})", p.blackout_id),
                        None => p.blackout_id.clone(),
                    })
                    .collect();
                rlog!(
                    "entry rejected: trade {tid} paused (active blackouts: {})",
                    blackouts.join(", ")
                );
                return ActionResult::Rejected {
                    response: Response::error("trade paused", 423),
                    outcome: format!("rejected: paused [{}]", blackouts.join(",")),
                };
            }
            Ok(_) => {}
            Err(err) => {
                rlog_err!("KV list_pauses_for_trade: {err}");
                return ActionResult::Rejected {
                    response: Response::error("state error", 500),
                    outcome: "rejected: state-error".into(),
                };
            }
        }
    }

    // Retry gate — when the intent opts into multi-shot mode via
    // `max_retries`, the gate inspects prior attempts (cancel-and-
    // replace a still-pending one, reject a fresh placement when an
    // earlier attempt is still open, allow another placement when
    // earlier attempts have closed) and enforces the placement cap.
    // "Retry" here means re-entry into a setup after a prior fill
    // closed (typically at SL), *not* a re-attempt of a failed
    // placement — broker failures are terminal and 502 out. See
    // `src/retry_gate.rs` for the full semantics. The single-shot
    // path (`max_retries: Static(0)`, the default) skips this branch
    // entirely so no new KV/broker calls land on the byte-identical
    // baseline.
    let retry_attempt_no = if !matches!(
        verified.intent.max_retries,
        trade_control_core::tunable::Tunable::Static(0)
    ) {
        match trade_control_core::retry_gate::evaluate(
            broker,
            store,
            &verified.intent,
            &verified.shell,
        )
        .await
        {
            trade_control_core::retry_gate::RetryGateOutcome::Proceed { next_attempt_no } => {
                Some(next_attempt_no)
            }
            trade_control_core::retry_gate::RetryGateOutcome::Rejected {
                status,
                message,
                outcome,
            } => {
                return ActionResult::Rejected {
                    response: Response::error(message, status),
                    outcome,
                };
            }
        }
    } else {
        None
    };

    // Cooldown gate — scoped to this intent's account so a cooldown on
    // a different account doesn't pause this one. A global cooldown
    // (set without `account:`) still pauses every account.
    match store
        .is_cooled_down(
            verified.intent.account.as_deref(),
            &verified.intent.instrument,
        )
        .await
    {
        Ok(true) => {
            rlog!(
                "entry rejected: {} cooled down (id={})",
                verified.intent.instrument,
                verified.intent.id
            );
            return ActionResult::Rejected {
                response: Response::error("instrument cooled down", 423),
                outcome: "rejected: cooled-down".into(),
            };
        }
        Ok(false) => {}
        Err(err) => {
            rlog_err!("KV is_cooled_down: {err}");
            return ActionResult::Rejected {
                response: Response::error("state error", 500),
                outcome: "rejected: state-error".into(),
            };
        }
    }

    // Prep gate — every name in `requires_preps` must be currently set,
    // and the stored `set_at` timestamps must be strictly increasing in
    // list order.
    let mut prev_ts: Option<chrono::DateTime<chrono::Utc>> = None;
    for step in &verified.intent.requires_preps {
        match store
            .get_prep(
                verified.intent.account.as_deref(),
                &verified.intent.instrument,
                step,
            )
            .await
        {
            Ok(Some(set_at)) => {
                if let Some(prev) = prev_ts
                    && set_at <= prev
                {
                    rlog!(
                        "entry rejected: prep {} not after previous (id={})",
                        step,
                        verified.intent.id
                    );
                    return ActionResult::Rejected {
                        response: Response::error("prep order violated", 412),
                        outcome: format!("rejected: prep-order-violated ({step})"),
                    };
                }
                prev_ts = Some(set_at);
            }
            Ok(None) => {
                rlog!(
                    "entry rejected: missing prep {} (id={})",
                    step,
                    verified.intent.id
                );
                return ActionResult::Rejected {
                    response: Response::error("missing prep", 412),
                    outcome: format!("rejected: missing-prep ({step})"),
                };
            }
            Err(err) => {
                rlog_err!("KV get_prep: {err}");
                return ActionResult::Rejected {
                    response: Response::error("state error", 500),
                    outcome: "rejected: state-error".into(),
                };
            }
        }
    }

    // Veto gate — entry is rejected if any opted-in veto is active.
    // Scope the check to the entry's `account` so a veto on a different
    // account doesn't block this trade; a global veto (set with no
    // `account:`) still blocks every account by design. The veto lookup
    // is also scoped to this entry's `trade_id` so a veto from a
    // different setup on the same instrument can't block it
    // (2026-06-11 fix). `Intent::validate` guarantees `trade_id` is
    // present on `enter`; the guard here is defence-in-depth.
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        rlog_err!(
            "enter missing trade_id at veto gate (id={})",
            verified.intent.id
        );
        return ActionResult::Rejected {
            response: Response::error("enter requires trade_id", 400),
            outcome: "rejected: missing-trade-id".into(),
        };
    };
    for veto in &verified.intent.vetos {
        match store
            .is_vetoed(
                verified.intent.account.as_deref(),
                trade_id,
                &verified.intent.instrument,
                veto,
            )
            .await
        {
            Ok(true) => {
                rlog!(
                    "entry rejected: veto {} active (id={})",
                    veto,
                    verified.intent.id
                );
                return ActionResult::Rejected {
                    response: Response::error("veto active", 412),
                    outcome: format!("rejected: veto-active ({veto})"),
                };
            }
            Ok(false) => {}
            Err(err) => {
                rlog_err!("KV is_vetoed: {err}");
                return ActionResult::Rejected {
                    response: Response::error("state error", 500),
                    outcome: "rejected: state-error".into(),
                };
            }
        }
    }

    let worker_max_risk_pct = secret_or_default(env, MAX_RISK_PCT_PER_TRADE_SECRET, 1.0);
    let worker_max_open_positions = secret_or_default(env, MAX_OPEN_POSITIONS_SECRET, 3.0) as u32;
    // Pip size precedence: the value baked into the signed intent at arm
    // time (the authority — `tv-arm` reads it from `instrument-lookup`) wins;
    // a missing field falls back to the per-instrument `PIP_SIZE_<instrument>`
    // secret, then the forex default. The fallback keeps any pre-baked
    // in-flight intent resolving during rollout. See `pip_size_for`.
    let pip_size = verified
        .intent
        .pip_size
        .unwrap_or_else(|| pip_size_for(env, &verified.intent.instrument));

    // M/W real-time geometry. For M/W enters carrying a `trade_id`, evolve
    // the live neckline / right-shoulder per bar (Phase B): a deeper body
    // still inside the 60% validity floor revises the neckline; a higher
    // body records the right shoulder (→ SL anchor); a body past the floor
    // cancels the setup (cancel pending + `mw-cancel` veto, never closes an
    // open position). All comparisons are body-based, so a rogue wick can't
    // move geometry or cancel. A bar with no `open` (pre-v2.5 chart) leaves
    // the state untouched and resolves against baked params. Returns the
    // effective `MwParams` to resolve this bar against, or short-circuits.
    let mw_effective = match maybe_update_mw_state(broker, store, verified, now).await {
        MwStateOutcome::Proceed(mw) => Some(mw),
        MwStateOutcome::NotMw => None,
        MwStateOutcome::Cancelled(result) => return result,
    };

    let resolve_result = match &mw_effective {
        // M/W with live geometry: resolve against the effective params.
        Some(mw) => Resolved::from_mw_intent(&verified.intent, &verified.shell, mw),
        // Everything else (and M/W with no trade_id / no `open`): the
        // standard dispatch, which itself routes baked M/W to from_mw_intent.
        None => Resolved::from_intent(&verified.intent, &verified.shell, pip_size),
    };
    let resolved = match resolve_result {
        Ok(r) => r,
        // An M/W bar that hasn't completed its real-time arming sequence is
        // a *benign, expected* decline ("stay armed for the next bar"), not a
        // bad request. Report it as a 200 with a distinct `declined:` outcome
        // so the timeline/verdict downstream can tell routine M/W declines
        // apart from a genuinely malformed enter. It is still a seen-id
        // `Skip` (Rejected), so the setup stays armed. See bug #7.
        Err(ResolveError::NotArmedYet) => {
            rlog!(
                "resolve: M/W not armed yet — declining this bar (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                response: Response::ok("declined: mw-not-armed"),
                outcome: "declined: mw-not-armed".into(),
            };
        }
        // Genuinely malformed enter (wrong-side SL/limit/stop, entry outside
        // SL..TP, sub-1R, missing field, bad script): a real 400 bad request.
        Err(err) => {
            rlog_err!("resolve: {err}");
            return ActionResult::Rejected {
                response: Response::error("rejected", 400),
                outcome: "rejected: resolve-failed".into(),
            };
        }
    };

    // Entry-level veto gate — Bug #12. The pcl-exhausted / invalidation level
    // is a *continuous* predicate: reject when the resolved entry price is
    // already past it, regardless of whether the engine's cross-event guard
    // fired or wrote a KV veto. The legacy persistent KV veto gave this
    // continuous semantics for free; the engine's one-shot Intrabar guard can
    // miss a gap / pre-armed breach and let the entry through (the NZD/CAD
    // −110.53 GBP incident). Sits after `resolved` (needs the entry price) and
    // before `allow_entry` (a regression-critical veto must not be defeatable
    // by an operator script). The `rejected: veto-active (<name>)` outcome is
    // byte-identical to the legacy KV veto path and is a seen-id `Skip`.
    let entry_ref_price = resolved.entry.reference_price();
    if let Some(elv) = verified
        .intent
        .entry_level_vetos
        .iter()
        .find(|elv| elv.is_past(entry_ref_price))
    {
        rlog!(
            "entry rejected: entry-level veto {} active (entry={entry_ref_price} past level={}) (id={})",
            elv.name,
            elv.level,
            verified.intent.id
        );
        return ActionResult::Rejected {
            response: Response::error("veto active", 412),
            outcome: format!("rejected: veto-active ({})", elv.name),
        };
    }

    // allow_entry gate — operator's Tunable<bool> script sees the full
    // shell + resolved geometry. Sits after Resolved::from_intent
    // (Phase 2 bindings need it) and ahead of the broker call (cheap
    // 412 on false). Doesn't consume a retry slot — only a successful
    // broker placement does.
    match allow_entry_gate::evaluate(&verified.intent, &verified.shell, &resolved, pip_size) {
        allow_entry_gate::AllowEntryOutcome::Proceed => {}
        allow_entry_gate::AllowEntryOutcome::Blocked => {
            rlog!(
                "entry rejected: allow_entry returned false (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                response: Response::error("entry blocked", 412),
                outcome: "rejected: allow-entry-false".into(),
            };
        }
        allow_entry_gate::AllowEntryOutcome::NeedsGoldenUnmet => {
            rlog!(
                "entry rejected: needs_golden set but shell.golden != Some(true) (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                response: Response::error("entry blocked: needs-golden", 412),
                outcome: "rejected: needs-golden".into(),
            };
        }
        allow_entry_gate::AllowEntryOutcome::NeedsConfirmedUnmet => {
            rlog!(
                "entry rejected: needs_confirmed set but shell.signal_confirmed != Some(true) (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                response: Response::error("entry blocked: needs-confirmed", 412),
                outcome: "rejected: needs-confirmed".into(),
            };
        }
        allow_entry_gate::AllowEntryOutcome::ScriptError { kind, message } => {
            rlog_err!(
                "allow_entry script error (id={}): {message}",
                verified.intent.id
            );
            return ActionResult::Rejected {
                response: Response::error("entry blocked: script error", 412),
                outcome: format!("rejected: allow-entry-{kind}"),
            };
        }
    }

    let caps = load_account_caps(env, verified.intent.account.as_deref()).await;
    // Apply the per-account narrowing now that we have the caps: an
    // account record can tighten the worker-wide ceiling but never
    // relax it.
    let max_risk_pct = caps.resolve_max_risk_pct(worker_max_risk_pct);
    let max_open_positions = caps.resolve_max_open_positions(worker_max_open_positions);

    // Resolve the optional bar-based order expiry into a concrete
    // `cancel_at` *before* any broker work, so a bad `expiry_bars`
    // rejects (without poisoning the seen-id) rather than placing an
    // order we can't honour. `None` = no bar-expiry requested.
    let cancel_at = match verified.intent.expiry_bars.as_ref() {
        None => None,
        Some(tunable) => {
            let n = match resolve_phase1_u32("expiry-bars", Some(tunable), &verified.shell, 0) {
                Ok(n) => n,
                Err(outcome) => {
                    rlog!("entry rejected: {outcome} (id={})", verified.intent.id);
                    return ActionResult::Rejected {
                        response: Response::error("entry blocked: expiry-bars script", 412),
                        outcome,
                    };
                }
            };
            match trade_control_core::intent::resolve_cancel_at(
                n,
                &verified.shell,
                verified.intent.not_after,
            ) {
                Ok(ts) => Some(ts),
                Err(err) => {
                    rlog!(
                        "entry rejected: expiry-bars out of range (id={}): {err}",
                        verified.intent.id
                    );
                    return ActionResult::Rejected {
                        response: Response::error("entry blocked: expiry-bars out of range", 400),
                        outcome: "rejected: expiry-bars-out-of-range".into(),
                    };
                }
            }
        }
    };

    // Market-hours entry blackout (System 1, the reject gate): reject a
    // brand-new entry that fires inside this instrument's daily close→open
    // gap, so a resting stop order is never left to trigger on the reopen
    // liquidity gap (the incident this feature fixes). The per-instrument
    // UTC no-entry windows are derived once a day by the 06:00 UTC cron
    // (`src/cron/blackout_hours.rs`) from the broker's session hours and
    // stored in KV. This is a pure KV read + a minute-of-day comparison —
    // no broker round-trip — so it sits ahead of the (broker-touching)
    // spread-blackout gate below.
    //
    // REJECT, NOT a delay (same discipline as spread-blackout): no KV
    // write, no re-fire scheduled. The next signal bar re-triggers and
    // re-runs this check — once the market has reopened the same entry
    // passes. Returning `ActionResult::Rejected` is a `Skip` in
    // `seen_decision` (no `mark_seen`), so this reject never poisons the
    // intent id; the in-hours refire is allowed through. See CLAUDE.md
    // "Replay protection scope". Do NOT add any KV write on this path.
    //
    // FAIL OPEN: a KV read hiccup, or an instrument with no derived
    // windows (24h / unparseable / not-yet-refreshed), must never block a
    // legitimate entry — `get_blackout_windows` returns an empty Vec in
    // those cases and `is_inside_any` is then always `false`.
    match store.get_blackout_windows(&resolved.instrument).await {
        Err(err) => {
            rlog_err!(
                "market-blackout: windows read failed for {} (id={}): {err} — failing open (allowing entry)",
                resolved.instrument,
                verified.intent.id
            );
        }
        Ok(windows) => {
            let now_min = market_blackout::now_utc_minute_of_day(now);
            if is_inside_any(now_min, &windows) {
                rlog!(
                    "entry rejected: market-blackout instrument={} now_utc_min={now_min} windows={windows:?} (id={})",
                    resolved.instrument,
                    verified.intent.id
                );
                return ActionResult::Rejected {
                    response: Response::error("entry blocked: market-hours blackout", 423),
                    outcome: "rejected: market-blackout".into(),
                };
            }
        }
    }

    // System 1 of the spread blackout: reject a brand-new entry that
    // fires during the post-NY-close liquidity trough when the live
    // spread on THIS instrument is elevated. Runs here — after every
    // gate (retry/cooldown/prep/veto/allow_entry) and `Resolved::from_intent`,
    // immediately before the broker order. The pure decision lives in
    // `spread_blackout::spread_blackout_decision`; this is the thin
    // KV-read + quote-sample wrapper around it.
    //
    // REJECT, NOT a delay: we do not persist anything, do not schedule a
    // re-fire, and do not touch KV here. The next legitimate signal bar
    // re-triggers the alert and re-runs this check — by then the spread
    // may have recovered and the same entry passes. Stateless + idempotent.
    //
    // SEEN-ID: returning `ActionResult::Rejected` is a `Skip` in
    // `seen_decision` (no `mark_seen`), so this reject does NOT poison the
    // intent id — the next fire is allowed through. See CLAUDE.md
    // "Replay protection scope". Do NOT add any KV write on this path.
    match store.get_spread_blackout_window().await {
        // Fail open on a transient KV read error — a blackout-window read
        // hiccup must never block a legitimate entry.
        Err(err) => {
            rlog_err!(
                "spread-blackout: window read failed (id={}): {err} — failing open (allowing entry)",
                verified.intent.id
            );
        }
        // Window closed — the overwhelmingly common path. Fall through
        // WITHOUT a broker round-trip (no `get_quote` call).
        Ok(None) => {}
        // Window open — sample the live spread for this instrument and
        // decide. A fine-spread instrument/day is not blacked out.
        Ok(Some(_window)) => match broker.get_quote(&resolved.instrument).await {
            // Fail open on a quote error at decision time: a transient
            // broker quote hiccup must not strand a real entry. (A
            // fail-closed variant is recorded in the sub-plan open
            // questions; flip this branch to reject if demo shows the
            // trough also degrades the quote endpoint.)
            Err(err) => {
                rlog_err!(
                    "spread-blackout: get_quote failed for {} (id={}): {err:?} — failing open (allowing entry)",
                    resolved.instrument,
                    verified.intent.id
                );
            }
            Ok(quote) => {
                let spread_pips = quote.spread() / pip_size;
                let threshold = spread_blackout::elevated_threshold_pips(&resolved.instrument);
                if spread_blackout::spread_blackout_decision(true, spread_pips, threshold) {
                    // Name the instrument's baked normal/spike so the
                    // operator can judge whether the block is right. Baked
                    // figures come from the spread-sampler baseline; absent
                    // for an uncatalogued instrument (then we only have the
                    // flat threshold to show).
                    let normal = match spread_blackout::baked_baseline(&resolved.instrument) {
                        Some((low, high, median)) => format!(
                            "{} normal spread ~{median:.1}p (seen {low:.1}–{high:.1}p)",
                            resolved.instrument
                        ),
                        None => format!("{} (no baseline)", resolved.instrument),
                    };
                    let message = format!(
                        "entry blocked: spread blackout — {normal}, current spread {spread_pips:.1}p > {threshold:.1}p; preventing entry for safety"
                    );
                    rlog!(
                        "entry rejected: spread-blackout instrument={} spread={spread_pips:.1}p > {threshold:.1}p (id={})",
                        resolved.instrument,
                        verified.intent.id
                    );
                    return ActionResult::Rejected {
                        response: Response::error(&message, 423),
                        outcome: "rejected: spread-blackout".into(),
                    };
                }
            }
        },
    }

    // SL-vs-spread floor (hard limit, every entry): the stop-loss distance must
    // be at least `SL_MIN_SPREAD_MULTIPLE`× the live bid-ask spread, so a stop
    // is a real market level and not dominated by the cost of crossing the book.
    // Pure decision in `trade_control_core::intent::sl_spread_floor_violation`;
    // this is the live-quote wrapper. Mirrored at arm/build time (tv-arm,
    // trade-control) so a bad setup is caught before signing — this is the
    // real-time backstop.
    //
    // Unlike spread-blackout this samples the quote on EVERY entry (no window
    // guard), since the floor always applies. It is the only other broker
    // round-trip on the entry path; keep it right beside spread-blackout.
    //
    // FAIL OPEN on a quote error: a transient broker quote hiccup must not
    // strand a legitimate entry (same discipline as spread-blackout). REJECT is
    // a `Skip` in `seen_decision` (no `mark_seen`), so it never poisons the
    // intent id — the next signal bar refires and re-checks. Do NOT add a KV
    // write on this path.
    match broker.get_quote(&resolved.instrument).await {
        Err(err) => {
            rlog_err!(
                "sl-spread-floor: get_quote failed for {} (id={}): {err:?} — failing open (allowing entry)",
                resolved.instrument,
                verified.intent.id
            );
        }
        Ok(quote) => {
            let spread_price = quote.spread();
            let sl_distance = (entry_reference_price(&resolved.entry) - resolved.stop_loss).abs();
            if trade_control_core::intent::sl_spread_floor_violation(sl_distance, spread_price) {
                let min_sl = trade_control_core::intent::SL_MIN_SPREAD_MULTIPLE * spread_price;
                // Render the distances in pips for the operator-facing message.
                // `pip_size` is the baked intent value (or the secret/default
                // fallback); guard against a non-positive divisor so a bad pip
                // never produces a NaN/inf in the reject body.
                let (sl_pips, spread_pips) = if pip_size > 0.0 {
                    (sl_distance / pip_size, spread_price / pip_size)
                } else {
                    (sl_distance, spread_price)
                };
                let message = format!(
                    "entry blocked: SL <= {mult:.0}x spread: SL distance {sl_pips:.1} pips; spread = {spread_pips:.1} pips",
                    mult = trade_control_core::intent::SL_MIN_SPREAD_MULTIPLE,
                );
                rlog!(
                    "entry rejected: sl-below-10x-spread instrument={} sl_distance={sl_distance} < {min_sl} (spread={spread_price}, {}x; {sl_pips:.1} pips vs {spread_pips:.1} pips) (id={})",
                    resolved.instrument,
                    trade_control_core::intent::SL_MIN_SPREAD_MULTIPLE,
                    verified.intent.id
                );
                return ActionResult::Rejected {
                    response: Response::error(&message, 422),
                    outcome: "rejected: sl-below-10x-spread".into(),
                };
            }
        }
    }

    let entry_request = EntryRequest {
        instrument: &resolved.instrument,
        direction: resolved.direction,
        entry: resolved.entry.clone(),
        stop_loss: resolved.stop_loss,
        take_profit: resolved.take_profit,
        risk: resolved.risk,
        dry_run: resolved.dry_run,
    };

    // Log inputs + R-multiple up front so the operator sees the
    // planned trade geometry before the broker work begins. The
    // broker's own `sizing:` log then adds the computed units once
    // equity / FX have been fetched.
    let r_distance = (entry_reference_price(&resolved.entry) - resolved.stop_loss).abs();
    let tp_distance = (resolved.take_profit - entry_reference_price(&resolved.entry)).abs();
    let r_multiple = if r_distance > 0.0 {
        tp_distance / r_distance
    } else {
        f64::NAN
    };
    let prefix = if resolved.dry_run { "DRY-RUN " } else { "" };
    rlog!(
        "{prefix}entry id={} instrument={} direction={:?} entry={:?} sl={} tp={} risk={:?} r={:.3}",
        verified.intent.id,
        resolved.instrument,
        resolved.direction,
        resolved.entry,
        resolved.stop_loss,
        resolved.take_profit,
        resolved.risk,
        r_multiple,
    );

    // First placement. On `EntryTooCloseToMarket` (TN `#19-10`), the
    // stop trigger was overtaken by price; the optional `recover_entry`
    // policy may recover with a *single* synchronous market re-place
    // (never a loop — a too-close means price is moving). The re-place
    // is the SAME intended entry, so it shares `retry_attempt_no` and
    // does not consume an extra multi-shot slot.
    let placement = match broker
        .place_entry(max_risk_pct, max_open_positions, &entry_request)
        .await
    {
        Ok(order_id) => Ok(order_id),
        Err(EntryError::EntryTooCloseToMarket) => {
            place_entry_too_close_fallback(
                broker,
                &resolved,
                &verified.intent.id,
                max_risk_pct,
                max_open_positions,
            )
            .await
        }
        Err(err) => Err(err),
    };

    match placement {
        Ok(order_id) => {
            if resolved.dry_run {
                rlog!("DRY-RUN entry id={} (not placed)", verified.intent.id);
                ActionResult::Ok(format!("dry-run: id={}", verified.intent.id))
            } else {
                rlog!("entry placed id={} order={}", verified.intent.id, order_id);
                if let Some(attempt_no) = retry_attempt_no {
                    // Break-even snapshot: only when the enter carried a
                    // `breakeven` rule AND we know the trade's timeframe (engine
                    // path). The cron joins the open position back to this row,
                    // fetches closed candles at `granularity`, and moves the SL
                    // to entry once a candle closes past 50%-to-TP.
                    let breakeven_snapshot = match (resolved.breakeven, enter_granularity) {
                        (Some(rule), Some(granularity)) => {
                            Some(trade_control_core::state::BreakevenSnapshot {
                                rule,
                                entry_price: resolved.entry.reference_price(),
                                take_profit: resolved.take_profit,
                                granularity,
                            })
                        }
                        _ => None,
                    };
                    trade_control_core::retry_gate::record_placement(
                        store,
                        &verified.intent,
                        verified.shell.time,
                        verified.intent.not_after,
                        now,
                        attempt_no,
                        &order_id,
                        resolved.direction,
                        resolved.stop_loss,
                        cancel_at,
                        breakeven_snapshot,
                    )
                    .await;
                }
                // Spread-blackout System 3 (Sub-plan 5): persist the raw signed
                // body keyed by the broker order id so the apply cron can
                // recover THIS order's intent (it finds a broker pending order,
                // never a signed intent) and re-drive it on recovery. Only when
                // we have the signed bytes in hand. No TTL — the body is
                // per-trade lifecycle state and is removed by `plan purge`
                // (no longer aged out with its EntryAttempt). Best-effort: a
                // write failure only costs the blackout-restore ability for this
                // one order, never the placement.
                if let Some(body) = raw_body
                    && let Err(err) = store.put_order_body(&order_id, body).await
                {
                    rlog_err!(
                        "order-body store for blackout-restore failed (order={order_id}): {err} \
                         — this order can't be blackout-cancelled+restored"
                    );
                }
                ActionResult::Ok(format!("entered: order={order_id}"))
            }
        }
        Err(err) => {
            // Stays `ActionResult::Failed` (a Skip in `seen_decision`):
            // a too-close / broker failure must never poison the seen-id
            // so the next signal bar can retry. The too-close case gets
            // a distinct outcome string for log-grep observability.
            let outcome = recover_entry::outcome_for_entry_error(&err);
            rlog_err!("entry failed: {err} ({outcome})");
            ActionResult::Failed(outcome)
        }
    }
}

/// Result of the per-bar M/W geometry update ([`maybe_update_mw_state`]).
enum MwStateOutcome {
    /// Not an M/W enter with a `trade_id`, or the bar carried no `open`:
    /// resolve against the baked params (the standard dispatch).
    NotMw,
    /// M/W setup still valid; resolve this bar against these effective
    /// (live-corrected) params.
    Proceed(trade_control_core::intent::MwParams),
    /// The setup was cancelled this bar (60% validity floor breached).
    /// Carries the terminal [`ActionResult`] the caller should return.
    Cancelled(ActionResult),
}

/// Evolve the live M/W geometry for this bar and decide how to resolve.
///
/// Only acts on M/W enters that carry a `trade_id` (the KV state is
/// trade-scoped). Reads the prior [`MwState`], runs the pure
/// [`plan_mw_update`], and:
///
/// - **Proceed** → persists the updated state (when it changed) and returns
///   the effective [`MwParams`][trade_control_core::intent::MwParams] to
///   resolve against.
/// - **Cancel** → cancels any pending order for the instrument, writes a
///   trade-scoped `mw-cancel` veto (so later fires of this `05-enter` are
///   blocked — it lists `mw-cancel` in its `vetos`), clears the state row,
///   and returns a rejection. It **never closes an open position** — the
///   veto is StopNextEntry-class; cancelling pending is the only broker
///   side effect (see `veto_close_only_when_thesis_invalidated`).
/// - **NoChange / NotMw** → `NotMw`, falling back to baked resolution.
///
/// Fail-soft: a KV read/write error logs and falls back to baked geometry
/// rather than blocking a legitimate entry.
async fn maybe_update_mw_state<B: Broker>(
    broker: &B,
    store: &impl StateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> MwStateOutcome {
    let intent = &verified.intent;
    let Some(mw) = intent.mw else {
        return MwStateOutcome::NotMw;
    };
    let Some(trade_id) = intent.trade_id.as_deref() else {
        // No trade_id → no trade-scoped state to evolve; baked resolution.
        return MwStateOutcome::NotMw;
    };
    let Some(direction) = intent.direction else {
        return MwStateOutcome::NotMw;
    };
    let account = intent.account.as_deref();

    let prior = match store.get_mw_state(account, trade_id).await {
        Ok(p) => p,
        Err(err) => {
            // Fail-soft: don't block a valid entry on a KV blip.
            rlog_err!("mw-state get failed (trade_id={trade_id}): {err} — using baked geometry");
            return MwStateOutcome::NotMw;
        }
    };

    let ttl_seconds = veto_ttl_seconds(0, intent.not_after, now);
    let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
    let anchors = MwAnchors {
        direction,
        runup_start: mw.runup_start,
        left_shoulder: mw.first_point,
        baked_neckline: mw.neckline,
        drawn_right_shoulder: mw.right_shoulder,
    };

    match plan_mw_update(anchors, prior, &verified.shell, now, expires_at) {
        MwUpdate::NoChange => MwStateOutcome::NotMw,
        MwUpdate::Proceed { state, changed } => {
            if changed {
                if let Err(err) = store
                    .upsert_mw_state(account, trade_id, &state, ttl_seconds)
                    .await
                {
                    // Persist failure is non-fatal: we still resolve this bar
                    // against the freshly-computed geometry; next bar re-derives
                    // from the prior row (or baked if the write never lands).
                    rlog_err!("mw-state upsert failed (trade_id={trade_id}): {err}");
                }
                rlog!(
                    "mw-state updated trade_id={trade_id} neckline={} right_shoulder={:?}",
                    state.neckline,
                    state.right_shoulder
                );
            }
            MwStateOutcome::Proceed(effective_mw_params(&mw, &state, direction))
        }
        MwUpdate::Cancel => {
            let cancelled = broker
                .cancel_pending_for_instrument(&intent.instrument)
                .await;
            if let Err(err) = store
                .set_veto(
                    account,
                    trade_id,
                    &intent.instrument,
                    MW_CANCEL_VETO_NAME,
                    ttl_seconds,
                )
                .await
            {
                rlog_err!("mw-state cancel: set_veto failed (trade_id={trade_id}): {err}");
            }
            record_control_event_for(
                store,
                account,
                Some(trade_id),
                trade_control_core::control_event::ControlKind::Veto,
                MW_CANCEL_VETO_NAME,
                &intent.instrument,
                ttl_seconds,
                now,
                None,
            )
            .await;
            // Clear the state row so a re-armed setup reusing the trade_id
            // starts clean. Best-effort.
            if let Err(err) = store.clear_mw_state(account, trade_id).await {
                rlog_err!("mw-state cancel: clear failed (trade_id={trade_id}): {err}");
            }
            rlog!(
                "mw-state CANCEL trade_id={trade_id} instrument={} account={} cancelled={cancelled} pending; mw-cancel veto set",
                intent.instrument,
                account.unwrap_or("<global>")
            );
            MwStateOutcome::Cancelled(ActionResult::Rejected {
                response: Response::error("mw pattern cancelled (validity floor breached)", 412),
                outcome: "rejected: mw-cancel (validity-floor)".into(),
            })
        }
    }
}

/// Single synchronous market re-place for a stop-entry rejected with
/// `#19-10` ("entry too close to / wrong side of market"). Reads the
/// current market price, applies the `recover_entry` slippage guard
/// (pure [`recover_entry::recover_entry_plan`]), and on a within-threshold
/// `market` action re-places as a **market order** sized against the
/// actual fill reference — a worse market fill changes the stop distance
/// and therefore the 1%-equity position size, so the broker re-runs
/// sizing from the market reference rather than the stop-trigger math.
///
/// For `action: limit` it instead re-places a **limit** order resting at
/// the original trigger (after a geometry guard — a limit on the wrong
/// side would be a `#19-9`), preserving the planned R and waiting for a
/// pullback. No fresh sizing: the entry reference is unchanged. The
/// resting limit is recorded as a normal `EntryAttempt` by the caller, so
/// the cron sweep cancels it when the alert window / `expiry_bars` lapses
/// — no broker-native GTD required.
///
/// Returns the original [`EntryError::EntryTooCloseToMarket`] (so the
/// caller surfaces the distinct outcome) when the fallback is absent,
/// out of threshold, `skip`, a wrong-side `limit`, or the re-place
/// itself fails / the price read fails. One attempt only.
async fn place_entry_too_close_fallback<B: Broker>(
    broker: &B,
    resolved: &trade_control_core::intent::Resolved,
    intent_id: &str,
    max_risk_pct: f64,
    max_open_positions: u32,
) -> Result<String, EntryError> {
    use trade_control_core::intent::ResolvedEntry;

    // Only stop entries carry the fallback; a too-close on anything else
    // (shouldn't happen) is terminal.
    let trigger_price = match &resolved.entry {
        ResolvedEntry::Stop { trigger_price } => *trigger_price,
        _ => return Err(EntryError::EntryTooCloseToMarket),
    };

    // The current price drives both the slippage guard and the new
    // market reference. A failed read is "price unavailable" → skip.
    let current_price = match broker.get_current_price(&resolved.instrument).await {
        Ok(p) => p,
        Err(err) => {
            rlog_err!(
                "too-close fallback: get_current_price({}) failed: {err} (id={intent_id})",
                resolved.instrument
            );
            return Err(EntryError::EntryTooCloseToMarket);
        }
    };

    match recover_entry::recover_entry_plan(
        resolved.recover_entry.as_ref(),
        resolved.direction,
        trigger_price,
        current_price,
    ) {
        recover_entry::RecoverEntryPlan::Skip { reason } => {
            rlog!(
                "too-close fallback: not recovering (id={intent_id} reason={reason} trigger={trigger_price} price={current_price})"
            );
            Err(EntryError::EntryTooCloseToMarket)
        }
        recover_entry::RecoverEntryPlan::Market { reference_price } => {
            rlog!(
                "too-close fallback: re-placing as MARKET (id={intent_id} trigger={trigger_price} price={reference_price})"
            );
            // Re-size against the actual fill reference: build a fresh
            // request whose entry is a market order at the current
            // price. The broker computes stop_distance from this
            // reference (TN re-fetches live bid/ask; OANDA uses it
            // directly), so the position size reflects the worse fill.
            let market_request = EntryRequest {
                instrument: &resolved.instrument,
                direction: resolved.direction,
                entry: ResolvedEntry::Market { reference_price },
                stop_loss: resolved.stop_loss,
                take_profit: resolved.take_profit,
                risk: resolved.risk,
                dry_run: resolved.dry_run,
            };
            match broker
                .place_entry(max_risk_pct, max_open_positions, &market_request)
                .await
            {
                Ok(order_id) => {
                    rlog!(
                        "too-close fallback: market re-place succeeded (id={intent_id} order={order_id})"
                    );
                    Ok(order_id)
                }
                Err(err) => {
                    // One attempt only — do not loop. Surface the
                    // original too-close identity so telemetry shows the
                    // recovery was attempted and failed, and the seen-id
                    // stays un-poisoned for the next bar.
                    rlog_err!("too-close fallback: market re-place failed: {err} (id={intent_id})");
                    Err(EntryError::EntryTooCloseToMarket)
                }
            }
        }
        recover_entry::RecoverEntryPlan::Limit { trigger_price } => {
            rlog!(
                "too-close fallback: re-placing as LIMIT at original trigger (id={intent_id} trigger={trigger_price} price={current_price})"
            );
            // The entry reference is unchanged (the limit rests at the
            // original trigger), so the stop distance — and therefore the
            // 1%-equity sizing — is identical to the original plan. Reuse
            // the resolved stop/take-profit/risk verbatim; the broker
            // sizes from the limit trigger just as it would have from the
            // stop trigger.
            let limit_request = EntryRequest {
                instrument: &resolved.instrument,
                direction: resolved.direction,
                entry: ResolvedEntry::Limit { trigger_price },
                stop_loss: resolved.stop_loss,
                take_profit: resolved.take_profit,
                risk: resolved.risk,
                dry_run: resolved.dry_run,
            };
            match broker
                .place_entry(max_risk_pct, max_open_positions, &limit_request)
                .await
            {
                Ok(order_id) => {
                    rlog!(
                        "too-close fallback: limit re-place succeeded (id={intent_id} order={order_id})"
                    );
                    Ok(order_id)
                }
                Err(err) => {
                    // One attempt only. Surface the original too-close
                    // identity so the seen-id stays un-poisoned and the
                    // next bar can retry.
                    rlog_err!("too-close fallback: limit re-place failed: {err} (id={intent_id})");
                    Err(EntryError::EntryTooCloseToMarket)
                }
            }
        }
    }
}

/// Reference price for risk math — for market orders it's the close,
/// for stop/limit it's the trigger. Same pick the broker layer uses.
fn entry_reference_price(entry: &trade_control_core::intent::ResolvedEntry) -> f64 {
    use trade_control_core::intent::ResolvedEntry;
    match entry {
        ResolvedEntry::Market { reference_price } => *reference_price,
        ResolvedEntry::Stop { trigger_price } => *trigger_price,
        ResolvedEntry::Limit { trigger_price } => *trigger_price,
    }
}

/// Handle the `status` action: dump cooldown + recent-seen indexes as YAML.
async fn handle_status(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let snap = match store.snapshot().await {
        Ok(s) => s,
        Err(err) => {
            rlog_err!("KV snapshot: {err}");
            return Response::error("state error", 500);
        }
    };
    let body = match serde_yaml::to_string(&snap) {
        Ok(s) => s,
        Err(err) => {
            rlog_err!("snapshot serialise: {err}");
            return Response::error("internal error", 500);
        }
    };
    record_seen(store, verified, now, "status").await;
    Response::ok(body)
}

/// Best-effort wrapper around `mark_seen`. Used by the dedicated control
/// handlers (status / unlock / prep / veto / clear-*) so each one ends
/// with one line instead of an `if let Err` repeated everywhere.
pub(crate) async fn record_seen<S: StateStore>(
    store: &S,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
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
        rlog_err!("KV mark_seen ({outcome}): {err}");
    }
}

/// Append a [`ControlEvent`] audit row alongside a TTL'd control set.
///
/// Best-effort and **non-blocking**: a failure is logged and swallowed — the
/// live control row was already set, and the audit trail must never gate it.
/// Skipped when there's no `trade_id` to scope it to (the trail is per-trade;
/// a `cooldown`/blackout set without a trade_id can't be journaled per trade).
/// `request_id` links the event back to its R2 `req/` bundle when known.
#[allow(clippy::too_many_arguments)]
async fn record_control_event_for<S: StateStore>(
    store: &S,
    account: Option<&str>,
    trade_id: Option<&str>,
    kind: trade_control_core::control_event::ControlKind,
    name: &str,
    instrument: &str,
    ttl_seconds: u64,
    now: chrono::DateTime<chrono::Utc>,
    request_id: Option<String>,
) {
    let Some(trade_id) = trade_id else {
        return;
    };
    let event = trade_control_core::control_event::ControlEvent::new(
        kind,
        name,
        instrument,
        now,
        ttl_seconds,
        request_id,
    );
    if let Err(err) = store.record_control_event(account, trade_id, &event).await {
        rlog_err!("KV record_control_event ({}/{name}): {err}", kind.tag());
    }
}

/// Handle the `unlock` action: clear the cooldown for `verified.intent.instrument`.
async fn handle_unlock(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let instrument = &verified.intent.instrument;
    let account = verified.intent.account.as_deref();
    let was = match store.clear_cooldown(account, instrument).await {
        Ok(b) => b,
        Err(err) => {
            rlog_err!("KV clear_cooldown: {err}");
            return Response::error("state error", 500);
        }
    };
    rlog!(
        "unlock instrument={instrument} account={} was_cooled_down={was}",
        account.unwrap_or("<global>")
    );
    let body = match serde_yaml::to_string(&UnlockResponse {
        unlocked: instrument.clone(),
        was_cooled_down: was,
    }) {
        Ok(s) => s,
        Err(err) => {
            rlog_err!("unlock serialise: {err}");
            return Response::error("internal error", 500);
        }
    };
    let outcome = if was { "unlocked" } else { "unlocked: noop" };
    record_seen(store, verified, now, outcome).await;
    Response::ok(body)
}

/// Handle the `prep` action: record a named step for an instrument with a TTL.
async fn handle_prep(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(step) = verified.intent.step.as_deref() else {
        return Response::error("prep requires `step`", 400);
    };
    let ttl_hours = match resolve_phase1_u32(
        "ttl_hours",
        Some(&verified.intent.ttl_hours),
        &verified.shell,
        0,
    ) {
        Ok(n) => n,
        Err(_outcome) => {
            return Response::error("ttl_hours script error", 412);
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
            rlog!(
                "prep rejected — expired: instrument={} account={} step={} trade_id={} \
                 (a {step}-expiry line already fired)",
                verified.intent.instrument,
                account.unwrap_or("<global>"),
                step,
                verified.intent.trade_id.as_deref().unwrap_or("<none>"),
            );
            return Response::error(format!("prep-expired: {step}"), 409);
        }
        Ok(false) => {}
        Err(err) => {
            rlog_err!("KV is_prep_blocked: {err}");
            return Response::error("state error", 500);
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
            rlog_err!("KV clear_named_preps (in clears): {err}");
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
        rlog_err!("KV set_prep: {err}");
        return Response::error("state error", 500);
    }
    record_control_event_for(
        store,
        account,
        verified.intent.trade_id.as_deref(),
        trade_control_core::control_event::ControlKind::Prep,
        step,
        &verified.intent.instrument,
        ttl_seconds,
        now,
        None,
    )
    .await;
    rlog!(
        "prep set: instrument={} account={} step={} ttl={}h cleared={:?}",
        verified.intent.instrument,
        account.unwrap_or("<global>"),
        step,
        ttl_hours,
        cleared
    );
    let outcome = format_prep_set_outcome(step, ttl_hours, &cleared);
    record_seen(store, verified, now, &outcome).await;
    Response::ok("ok")
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
async fn handle_prep_expire(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(step) = verified.intent.step.as_deref() else {
        return Response::error("prep-expire requires `step`", 400);
    };
    let ttl_hours = match resolve_phase1_u32(
        "ttl_hours",
        Some(&verified.intent.ttl_hours),
        &verified.shell,
        0,
    ) {
        Ok(n) => n,
        Err(_outcome) => {
            return Response::error("ttl_hours script error", 412);
        }
    };
    let ttl_seconds = (ttl_hours as u64).saturating_mul(3600);
    let account = verified.intent.account.as_deref();
    if let Err(err) = store
        .block_prep(account, &verified.intent.instrument, step, now, ttl_seconds)
        .await
    {
        rlog_err!("KV block_prep: {err}");
        return Response::error("state error", 500);
    }
    // Timeline log (step 1 of the prep-expire flow): an operator
    // reconstructing a trade later greps `prep-expire stored` to see
    // when the cutoff fired.
    rlog!(
        "prep-expire stored: instrument={} account={} step={} ttl={}h trade_id={}",
        verified.intent.instrument,
        account.unwrap_or("<global>"),
        step,
        ttl_hours,
        verified.intent.trade_id.as_deref().unwrap_or("<none>"),
    );
    let outcome = format!("prep-expire: {step} blocked ttl={ttl_hours}h");
    record_seen(store, verified, now, &outcome).await;
    Response::ok("ok")
}

/// Handle the `veto` action at level `stop-next-entry`: record a named
/// veto for an instrument with a TTL. No broker call.
async fn handle_veto(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(name) = verified.intent.name.as_deref() else {
        return Response::error("veto requires `name`", 400);
    };
    // `Intent::validate` guarantees `trade_id` on `veto`; guard here is
    // defence-in-depth. The veto key is scoped per-setup so it can't
    // bleed into a different setup on the same instrument.
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return Response::error("veto requires trade_id", 400);
    };
    let ttl_hours = match resolve_phase1_u32(
        "ttl_hours",
        Some(&verified.intent.ttl_hours),
        &verified.shell,
        0,
    ) {
        Ok(n) => n,
        Err(_outcome) => return Response::error("ttl_hours script error", 412),
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
            rlog_err!("KV clear_named_vetos (in clears): {err}");
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
        rlog_err!("KV set_veto: {err}");
        return Response::error("state error", 500);
    }
    record_control_event_for(
        store,
        account,
        Some(trade_id),
        trade_control_core::control_event::ControlKind::Veto,
        name,
        &verified.intent.instrument,
        ttl_seconds,
        now,
        None,
    )
    .await;
    rlog!(
        "veto set: instrument={} account={} name={} ttl={}h cleared={:?}",
        verified.intent.instrument,
        account.unwrap_or("<global>"),
        name,
        ttl_hours,
        cleared
    );
    let outcome = format_veto_set_outcome(name, ttl_hours, "stop-next-entry", &cleared, None, None);
    record_seen(store, verified, now, &outcome).await;
    Response::ok("ok")
}

/// Format the seen-index outcome string for a veto. Used by both the
/// flag-only path (`handle_veto`) and the broker-side path
/// (`run_veto_with_broker`). `cancelled` is the count of pending orders
/// the broker cancelled (None for the flag-only path); `closed_tag` is
/// `"closed=ok"` / `"closed=failed"` when a close was attempted (or
/// None otherwise).
fn format_veto_set_outcome(
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
async fn run_veto_with_broker<B: Broker>(
    broker: &B,
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> ActionResult {
    let Some(name) = verified.intent.name.as_deref() else {
        return ActionResult::Rejected {
            response: Response::error("veto requires `name`", 400),
            outcome: "rejected: missing-name".into(),
        };
    };
    // `Intent::validate` guarantees `trade_id` on `veto`; guard here is
    // defence-in-depth (the veto key is scoped per-setup).
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return ActionResult::Rejected {
            response: Response::error("veto requires trade_id", 400),
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
                response: Response::error("ttl_hours script error", 412),
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
            rlog_err!("KV clear_named_vetos (in clears): {err}");
            Vec::new()
        }
    };
    if let Err(err) = store
        .set_veto(account, trade_id, instrument, name, ttl_seconds)
        .await
    {
        rlog_err!("KV set_veto: {err}");
        return ActionResult::Rejected {
            response: Response::error("state error", 500),
            outcome: "rejected: state-error".into(),
        };
    }
    record_control_event_for(
        store,
        account,
        Some(trade_id),
        trade_control_core::control_event::ControlKind::Veto,
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

    rlog!(
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

/// Handle the `clear-prep` action: drop a single prep flag.
async fn handle_clear_prep(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(step) = verified.intent.step.as_deref() else {
        return Response::error("clear-prep requires `step`", 400);
    };
    let account = verified.intent.account.as_deref();
    let cleared_setter = match store
        .clear_prep(account, &verified.intent.instrument, step)
        .await
    {
        Ok(s) => s,
        Err(err) => {
            rlog_err!("KV clear_prep: {err}");
            return Response::error("state error", 500);
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
        rlog_err!("KV forget_seen({setter_id}): {err}");
    }
    let was = cleared_setter.is_some();
    rlog!(
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
    Response::ok("ok")
}

/// Handle the `clear-veto` action: drop a single veto flag.
async fn handle_clear_veto(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(name) = verified.intent.name.as_deref() else {
        return Response::error("clear-veto requires `name`", 400);
    };
    // `Intent::validate` guarantees `trade_id` on `clear-veto`; the veto
    // key is scoped per-setup so a clear only drops this setup's veto.
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return Response::error("clear-veto requires trade_id", 400);
    };
    let account = verified.intent.account.as_deref();
    let was = match store
        .clear_veto(account, trade_id, &verified.intent.instrument, name)
        .await
    {
        Ok(b) => b,
        Err(err) => {
            rlog_err!("KV clear_veto: {err}");
            return Response::error("state error", 500);
        }
    };
    rlog!(
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
    Response::ok("ok")
}

/// Handle the `pause` action: arm a blackout for `(trade_id, blackout_id)`.
/// No broker work. The KV entry's TTL is keyed off `not_after` (plus a
/// grace tail) so an orphaned pause from a dropped `resume` eventually
/// ages out instead of pinning the trade forever. The matching `resume`
/// is the authoritative clear.
async fn handle_pause(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return Response::error("pause requires `trade_id`", 400);
    };
    let Some(blackout_id) = verified.intent.blackout_id.as_deref() else {
        return Response::error("pause requires `blackout_id`", 400);
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
        rlog_err!("KV set_pause: {err}");
        return Response::error("state error", 500);
    }
    record_control_event_for(
        store,
        verified.intent.account.as_deref(),
        Some(trade_id),
        trade_control_core::control_event::ControlKind::Pause,
        blackout_id,
        &verified.intent.instrument,
        ttl_seconds,
        now,
        None,
    )
    .await;
    rlog!(
        "pause set: trade_id={trade_id} blackout_id={blackout_id} reason={:?}",
        reason
    );
    let outcome = match reason {
        Some(r) => format!("pause-set: {blackout_id} ({r})"),
        None => format!("pause-set: {blackout_id}"),
    };
    record_seen(store, verified, now, &outcome).await;
    Response::ok("ok")
}

/// Handle the `resume` action: clear a single `(trade_id, blackout_id)`
/// pause. Sibling blackouts on the same trade survive.
async fn handle_resume(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return Response::error("resume requires `trade_id`", 400);
    };
    let Some(blackout_id) = verified.intent.blackout_id.as_deref() else {
        return Response::error("resume requires `blackout_id`", 400);
    };
    let was = match store.clear_pause(trade_id, blackout_id).await {
        Ok(b) => b,
        Err(err) => {
            rlog_err!("KV clear_pause: {err}");
            return Response::error("state error", 500);
        }
    };
    rlog!("resume: trade_id={trade_id} blackout_id={blackout_id} was_set={was}");
    let outcome = if was {
        format!("pause-cleared: {blackout_id}")
    } else {
        format!("pause-cleared: {blackout_id} (noop)")
    };
    record_seen(store, verified, now, &outcome).await;
    Response::ok("ok")
}

/// Handle the `news-start` action: open a news window for
/// `(trade_id, news_id)`. No broker work. Mirrors `handle_pause` but
/// writes to the news-window KV namespace, which only the gated
/// `close` reads — entries are not blocked by news windows.
async fn handle_news_start(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return Response::error("news-start requires `trade_id`", 400);
    };
    let Some(news_id) = verified.intent.news_id.as_deref() else {
        return Response::error("news-start requires `news_id`", 400);
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
        rlog_err!("KV set_news_window: {err}");
        return Response::error("state error", 500);
    }
    record_control_event_for(
        store,
        verified.intent.account.as_deref(),
        Some(trade_id),
        trade_control_core::control_event::ControlKind::News,
        news_id,
        &verified.intent.instrument,
        ttl_seconds,
        now,
        None,
    )
    .await;
    rlog!(
        "news-start: trade_id={trade_id} news_id={news_id} reason={:?}",
        reason
    );
    let outcome = match reason {
        Some(r) => format!("news-start: {news_id} ({r})"),
        None => format!("news-start: {news_id}"),
    };
    record_seen(store, verified, now, &outcome).await;
    Response::ok("ok")
}

/// Handle the `news-end` action: close a single
/// `(trade_id, news_id)` news window.
async fn handle_news_end(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(trade_id) = verified.intent.trade_id.as_deref() else {
        return Response::error("news-end requires `trade_id`", 400);
    };
    let Some(news_id) = verified.intent.news_id.as_deref() else {
        return Response::error("news-end requires `news_id`", 400);
    };
    let was = match store.clear_news_window(trade_id, news_id).await {
        Ok(b) => b,
        Err(err) => {
            rlog_err!("KV clear_news_window: {err}");
            return Response::error("state error", 500);
        }
    };
    rlog!("news-end: trade_id={trade_id} news_id={news_id} was_set={was}");
    let outcome = if was {
        format!("news-end: {news_id}")
    } else {
        format!("news-end: {news_id} (noop)")
    };
    record_seen(store, verified, now, &outcome).await;
    Response::ok("ok")
}

/// Handle the `register` action: accept a server-side
/// [`TradePlan`](trade_control_core::trade_plan::TradePlan) for the engine to
/// evaluate on each cron tick. A control action — no broker work; idempotent
/// (re-registering refreshes the row), so it marks-seen on every completion
/// like the other control handlers.
///
/// **Stage C scope:** this validates the intent actually carries a plan and
/// that the plan's `trade_id` matches the intent's, then acknowledges it. The
/// KV persistence of the plan + its per-rule `PlanState` is Stage D — until
/// then a register is a logged no-op so the wire path and the dispatch routing
/// can be exercised end-to-end without the engine's storage schema. The
/// `not-yet-persisted` outcome string makes that explicit in `status`.
async fn handle_register(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(plan) = verified.intent.trade_plan.as_ref() else {
        return Response::error("register requires a `trade_plan`", 400);
    };
    // The plan and its carrier intent must agree on which trade they describe,
    // otherwise the engine couldn't key the plan's state to the intent's id.
    if let Some(intent_trade_id) = verified.intent.trade_id.as_deref()
        && intent_trade_id != plan.trade_id
    {
        return Response::error(
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
        rlog_err!(
            "register: put_trade_plan failed (trade_id={}): {err}",
            plan.trade_id
        );
        return Response::error("state error", 500);
    }
    rlog!(
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
    Response::ok("ok")
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
    granularity: trade_control_core::broker::Granularity,
    /// Observe-only? The thing an operator most wants to confirm during the
    /// engine's parallel-run period.
    shadow: bool,
    rules: usize,
    /// `PlanState`-derived fields. `None`/empty until the plan's first cron
    /// tick has seeded its state (a registered-but-not-yet-ticked plan has no
    /// state row).
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<trade_control_core::plan_state::Phase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    watermark: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fired: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retest_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Set only for an archived (terminated) plan — the time the engine archived
    /// it on its terminal cron tick. Absent on a live plan, which doubles as the
    /// CLI's "is this row terminated?" marker (`ARCHIVED` column). Surfaced only
    /// by `plan list --include-all`.
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The full `plan show` payload: the whole registered plan plus its engine
/// state, both serialised verbatim so the operator can inspect every rule and
/// the exact persisted state.
#[derive(Serialize)]
struct PlanDetail {
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    plan: trade_control_core::trade_plan::TradePlan,
    /// `None` until the first cron tick seeds the plan's state row.
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<trade_control_core::plan_state::PlanState>,
    /// `Some` when this match came from the archive (a terminated plan), `None`
    /// for a live registered plan. Mirrors [`PlanSummary::archived_at`] so the
    /// operator can tell at a glance whether `plan show` surfaced a live or a
    /// finished plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    archived_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Handle the `plan-list` action: enumerate every registered plan across all
/// account scopes, pair each with its current `PlanState`, and return a compact
/// YAML summary. Read-only, KV-only, idempotent (marks seen on completion like
/// every other control action).
async fn handle_plan_list(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let plans = match store.list_all_trade_plans().await {
        Ok(v) => v,
        Err(err) => {
            rlog_err!("plan-list: list_all_trade_plans: {err}");
            return Response::error("state error", 500);
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
                rlog_err!("plan-list: list_all_archived_plans: {err}");
                return Response::error("state error", 500);
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
            rlog_err!("plan-list serialise: {err}");
            return Response::error("internal error", 500);
        }
    };
    record_seen(
        store,
        verified,
        now,
        &format!("plan-list: {} plans", summaries.len()),
    )
    .await;
    Response::ok(body)
}

/// Build the compact summary for one stored plan + its (optional) state.
fn plan_summary(
    stored: &trade_control_core::state::StoredPlan,
    state: Option<trade_control_core::plan_state::PlanState>,
) -> PlanSummary {
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
fn archived_plan_summary(archived: &trade_control_core::state::ArchivedPlan) -> PlanSummary {
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
/// `MemStateStore`; `worker::Response` construction stays in the caller.
async fn collect_plan_details<S: StateStore>(
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
async fn handle_plan_show(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(target) = verified.intent.trade_id.as_deref() else {
        return Response::error("plan-show requires a `trade_id`", 400);
    };

    let details = match collect_plan_details(store, target).await {
        Ok(v) => v,
        Err(err) => {
            rlog_err!("plan-show: collect_plan_details: {err}");
            return Response::error("state error", 500);
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
        return Response::error(format!("no registered plan with trade_id {target}"), 404);
    }

    let body = match serde_yaml::to_string(&details) {
        Ok(s) => s,
        Err(err) => {
            rlog_err!("plan-show serialise: {err}");
            return Response::error("internal error", 500);
        }
    };
    record_seen(store, verified, now, &format!("plan-show: {target}")).await;
    Response::ok(body)
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
async fn handle_plan_delete(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(target) = verified.intent.trade_id.as_deref() else {
        return Response::error("plan-delete requires a `trade_id`", 400);
    };

    let plans = match store.list_all_trade_plans().await {
        Ok(v) => v,
        Err(err) => {
            rlog_err!("plan-delete: list_all_trade_plans: {err}");
            return Response::error("state error", 500);
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
            rlog_err!("plan-delete: clear_trade_plan({target}): {err}");
            return Response::error("state error", 500);
        }
        if let Err(err) = store.clear_plan_state(account, target).await {
            rlog_err!("plan-delete: clear_plan_state({target}): {err}");
            return Response::error("state error", 500);
        }
        deleted += 1;
        rlog!(
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
            rlog_err!("plan-delete: list_all_archived_plans: {err}");
            return Response::error("state error", 500);
        }
    };
    for stored in &archived {
        if stored.plan.trade_id != target {
            continue;
        }
        let account = stored.account.as_deref();
        if let Err(err) = store.clear_archived_plan(account, target).await {
            rlog_err!("plan-delete: clear_archived_plan({target}): {err}");
            return Response::error("state error", 500);
        }
        deleted += 1;
        rlog!(
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
    Response::ok(outcome)
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
async fn handle_plan_purge(
    store: &KvStateStore,
    env: &Env,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(target) = verified.intent.trade_id.as_deref() else {
        return Response::error("plan-purge requires a `trade_id`", 400);
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
    Response::ok(outcome)
}

/// Handle the `purge-older-than` action: bulk-delete R2 `req/` + `ticks/`
/// bundles whose date partition is strictly older than the cutoff carried in
/// `intent.not_before`. KV is untouched (per-trade KV rows are dropped by
/// `plan purge`). Manual retention housekeeping for the no-TTL recording bucket.
async fn handle_purge_older_than(
    env: &Env,
    verified: &incoming::Verified,
    _now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(cutoff) = verified.intent.not_before else {
        return Response::error("purge-older-than requires a cutoff in `not_before`", 400);
    };
    let deleted = match r2_purge::purge_older_than(env, cutoff).await {
        Ok(n) => n,
        Err(err) => {
            rlog_err!("purge-older-than: {err}");
            return Response::error("r2 purge error", 500);
        }
    };
    rlog!("purge-older-than: cutoff={cutoff} r2_deleted={deleted}");
    Response::ok(format!("purged-older-than: {cutoff} ({deleted})"))
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

#[cfg(test)]
mod dispatcher_outcome_tests {
    //! Pins the behaviour of [`record_dispatcher_outcome`] against the
    //! seen-by-id index. Only [`ActionResult::Ok`] writes; `Failed`
    //! and every flavour of `Rejected` are no-ops. See the function's
    //! own docs for the CHF/JPY 2026-06-02 motivation.
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use std::cell::RefCell;
    use trade_control_core::incoming::Verified;
    use trade_control_core::intent::{
        Action, BrokerKind, Direction, EntrySpec, Intent, PriceRef, Shell,
    };
    use trade_control_core::state::{EntryAttempt, Snapshot, StateError, StateStore};
    use trade_control_core::tunable::Tunable;

    /// Captures every `mark_seen` call. All other [`StateStore`]
    /// methods are stubbed out — the dispatcher-outcome path only
    /// touches `mark_seen`.
    #[derive(Default)]
    struct SeenSpyStore {
        marks: RefCell<Vec<(String, String)>>,
    }

    impl SeenSpyStore {
        fn marks(&self) -> Vec<(String, String)> {
            self.marks.borrow().clone()
        }
    }

    impl StateStore for SeenSpyStore {
        async fn is_seen(&self, _id: &str) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn mark_seen(
            &self,
            id: &str,
            _action: Action,
            _seen_at: DateTime<Utc>,
            outcome: &str,
            _ttl_seconds: u64,
            _trade_id: Option<&str>,
        ) -> Result<(), StateError> {
            self.marks
                .borrow_mut()
                .push((id.to_string(), outcome.to_string()));
            Ok(())
        }
        async fn forget_seen(&self, _id: &str) -> Result<(), StateError> {
            Ok(())
        }
        async fn is_cooled_down(
            &self,
            _account: Option<&str>,
            _instrument: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn set_cooldown(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _hours: u32,
            _now: DateTime<Utc>,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn clear_cooldown(
            &self,
            _account: Option<&str>,
            _instrument: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn set_prep(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _step: &str,
            _now: DateTime<Utc>,
            _ttl_seconds: u64,
            _setter_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_prep(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _step: &str,
        ) -> Result<Option<DateTime<Utc>>, StateError> {
            Ok(None)
        }
        async fn clear_prep(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _step: &str,
        ) -> Result<Option<String>, StateError> {
            Ok(None)
        }
        async fn set_veto(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _instrument: &str,
            _name: &str,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn is_vetoed(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _instrument: &str,
            _name: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn clear_veto(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _instrument: &str,
            _name: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn block_prep(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _step: &str,
            _now: chrono::DateTime<chrono::Utc>,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn is_prep_blocked(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _step: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn clear_prep_block(
            &self,
            _account: Option<&str>,
            _instrument: &str,
            _step: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn snapshot(&self) -> Result<Snapshot, StateError> {
            Ok(Snapshot {
                now: Utc::now(),
                cooldowns: Vec::new(),
                recent_seen: Vec::new(),
                preps: Vec::new(),
                vetos: Vec::new(),
                pauses: Vec::new(),
                news_windows: Vec::new(),
                prep_blocks: Vec::new(),
                spread_blackouts: Vec::new(),
                spread_blackout_window: None,
            })
        }
        async fn set_pause(
            &self,
            _trade_id: &str,
            _blackout_id: &str,
            _reason: Option<&str>,
            _now: DateTime<Utc>,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn list_pauses_for_trade(
            &self,
            _trade_id: &str,
        ) -> Result<Vec<trade_control_core::state::PauseEntry>, StateError> {
            Ok(Vec::new())
        }
        async fn clear_pause(
            &self,
            _trade_id: &str,
            _blackout_id: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn set_news_window(
            &self,
            _trade_id: &str,
            _news_id: &str,
            _reason: Option<&str>,
            _now: DateTime<Utc>,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn list_news_windows_for_trade(
            &self,
            _trade_id: &str,
        ) -> Result<Vec<trade_control_core::state::NewsEntry>, StateError> {
            Ok(Vec::new())
        }
        async fn clear_news_window(
            &self,
            _trade_id: &str,
            _news_id: &str,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn record_entry_attempt(&self, _attempt: EntryAttempt) -> Result<(), StateError> {
            Ok(())
        }
        async fn list_entry_attempts(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<Vec<EntryAttempt>, StateError> {
            Ok(Vec::new())
        }
        async fn set_entry_attempt_broker_trade_id(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _attempt_no: u32,
            _broker_trade_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn is_retry_fire_seen(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _shell_time: DateTime<Utc>,
        ) -> Result<bool, StateError> {
            Ok(false)
        }
        async fn mark_retry_fire_seen(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _shell_time: DateTime<Utc>,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn list_all_entry_attempts(&self) -> Result<Vec<EntryAttempt>, StateError> {
            Ok(Vec::new())
        }
        async fn delete_entry_attempt(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _attempt_no: u32,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn set_spread_blackout_window(
            &self,
            _now: DateTime<Utc>,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_spread_blackout_window(
            &self,
        ) -> Result<Option<trade_control_core::state::SpreadBlackoutWindow>, StateError> {
            Ok(None)
        }
        async fn set_blackout_windows(
            &self,
            _instrument: &str,
            _windows: &[trade_control_core::intent::NoEntryWindow],
            _now: DateTime<Utc>,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_blackout_windows(
            &self,
            _instrument: &str,
        ) -> Result<Vec<trade_control_core::intent::NoEntryWindow>, StateError> {
            Ok(Vec::new())
        }
        async fn upsert_spread_blackout_record(
            &self,
            _record: &trade_control_core::state::SpreadBlackoutRecord,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_spread_blackout_record(
            &self,
            _trade_id: &str,
        ) -> Result<Option<trade_control_core::state::SpreadBlackoutRecord>, StateError> {
            Ok(None)
        }
        async fn list_all_spread_blackout_records(
            &self,
        ) -> Result<Vec<trade_control_core::state::SpreadBlackoutRecord>, StateError> {
            Ok(Vec::new())
        }
        async fn clear_spread_blackout_record(&self, _trade_id: &str) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_mw_state(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<Option<trade_control_core::state::MwState>, StateError> {
            Ok(None)
        }
        async fn upsert_mw_state(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _state: &trade_control_core::state::MwState,
            _ttl_seconds: u64,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn clear_mw_state(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }

        // Engine plan/state methods — unused by these tests; minimal stubs.
        async fn put_trade_plan(
            &self,
            _account: Option<&str>,
            _plan: &trade_control_core::trade_plan::TradePlan,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_trade_plan(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<Option<trade_control_core::trade_plan::TradePlan>, StateError> {
            Ok(None)
        }
        async fn list_all_trade_plans(
            &self,
        ) -> Result<Vec<trade_control_core::state::StoredPlan>, StateError> {
            Ok(Vec::new())
        }
        async fn clear_trade_plan(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn get_plan_state(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<Option<trade_control_core::plan_state::PlanState>, StateError> {
            Ok(None)
        }
        async fn put_plan_state(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _state: &trade_control_core::plan_state::PlanState,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn clear_plan_state(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn record_control_event(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
            _event: &trade_control_core::control_event::ControlEvent,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn list_control_events(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<Vec<trade_control_core::control_event::ControlEvent>, StateError> {
            Ok(Vec::new())
        }
        async fn clear_control_events(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn archive_plan(
            &self,
            _account: Option<&str>,
            _plan: &trade_control_core::trade_plan::TradePlan,
            _final_state: &trade_control_core::plan_state::PlanState,
            _archived_at: DateTime<Utc>,
        ) -> Result<(), StateError> {
            Ok(())
        }
        async fn list_all_archived_plans(
            &self,
        ) -> Result<Vec<trade_control_core::state::ArchivedPlan>, StateError> {
            Ok(vec![])
        }
        async fn clear_archived_plan(
            &self,
            _account: Option<&str>,
            _trade_id: &str,
        ) -> Result<(), StateError> {
            Ok(())
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 2, 17, 0, 0).unwrap()
    }

    fn not_after() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 4, 16, 50, 53).unwrap()
    }

    fn verified(id: &str) -> Verified {
        Verified {
            shell: Shell {
                close: 203.0,
                high: 203.2,
                low: 202.9,
                open: None,
                time: now(),
                signal_high: None,
                signal_low: None,
                signal_range: None,
                signal_start_time: None,
                signal_kind: None,
                golden: None,
                atr: None,
                signal_confirmed: None,
                recent_high: None,
                recent_low: None,
                next_candle_timestamp_1: None,
                next_candle_timestamp_2: None,
                next_candle_timestamp_3: None,
                next_candle_timestamp_4: None,
                next_candle_timestamp_5: None,
            },
            intent: Intent {
                entry_level_vetos: Vec::new(),
                v: 1,
                id: id.into(),
                not_before: None,
                not_after: not_after(),
                action: Action::Enter,
                instrument: "CHF_JPY".into(),
                direction: Some(Direction::Short),
                entry: Some(EntrySpec::Market),
                stop_loss: Some(PriceRef::Absolute { absolute: 203.5 }),
                take_profit: None,
                risk_pct: Tunable::Static(0.25),
                risk_amount: None,
                size_units: None,
                dry_run: None,
                cooldown_hours: None,
                min_r: None,
                broker: BrokerKind::TradeNation,
                account: Some("reversals".into()),
                step: None,
                name: None,
                ttl_hours: Tunable::Static(0),
                level: None,
                requires_preps: Vec::new(),
                vetos: Vec::new(),
                clears: Vec::new(),
                trade_id: Some("hs-chf-jpy-test".into()),
                max_retries: Tunable::Static(0),
                expiry_bars: None,
                allow_entry: None,
                allow_close: None,
                needs_golden: false,
                blackout_id: None,
                news_id: None,
                require_news_window: None,
                require_price_in_ranges: None,
                needs_confirmed: false,
                inside_window: Vec::new(),
                sr_bands: Vec::new(),
                veto_on_reversal: false,
                reason: None,
                mw: None,
                pip_size: None,
                trade_plan: None,
                blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
                breakeven: None,
                include_archived: false,
            },
        }
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        pollster::block_on(f)
    }

    #[test]
    fn ok_outcome_classifies_as_mark() {
        let result = ActionResult::Ok("entered: order=42".into());
        assert_eq!(
            seen_decision(&result),
            SeenDecision::Mark {
                outcome: "entered: order=42"
            },
        );
    }

    #[test]
    fn failed_outcome_classifies_as_skip() {
        let result = ActionResult::Failed("entry-failed: broker 500".into());
        assert_eq!(
            seen_decision(&result),
            SeenDecision::Skip {
                kind: "failed",
                outcome: "entry-failed: broker 500"
            },
            "Failed must classify as Skip — broker errors don't poison the id",
        );
    }

    /// A too-close (`#19-10`) entry failure must classify as Skip so it
    /// never poisons the seen-id — the recovery contract is "let the
    /// next bar retry". Uses the exact string the worker emits via
    /// `recover_entry::outcome_for_entry_error`.
    #[test]
    fn too_close_outcome_classifies_as_skip() {
        let result = ActionResult::Failed("entry-failed: too-close-to-market".into());
        assert!(
            matches!(
                seen_decision(&result),
                SeenDecision::Skip { kind: "failed", .. }
            ),
            "too-close must Skip — recovery relies on the next bar retrying",
        );
    }

    /// End-to-end happy-path: an `Ok` outcome routed through the
    /// async helper actually lands in the store. Pins the wiring
    /// between `seen_decision::Mark` and the `store.mark_seen` call —
    /// without this, a future refactor could move the decision
    /// classification away from the store write and the
    /// classification tests above wouldn't catch it.
    #[test]
    fn ok_outcome_writes_to_store_via_record_dispatcher_outcome() {
        let store = SeenSpyStore::default();
        let v = verified("ok-id");
        let result = ActionResult::Ok("entered: order=42".into());
        run(record_dispatcher_outcome(&store, &v, now(), &result));
        assert_eq!(
            store.marks(),
            vec![("ok-id".into(), "entered: order=42".into())],
            "Ok must write to seen so duplicate alert bodies 409 on replay",
        );
    }

    /// End-to-end skip path: a `Failed` outcome routed through the
    /// async helper does NOT touch the store. We use `Failed` rather
    /// than `Rejected` because the `response` field of `Rejected` is
    /// a `worker::Result<worker::Response>` which calls into
    /// wasm-bindgen at construction and panics off-wasm; the
    /// classification test below covers the `Rejected` variant via
    /// `seen_decision`.
    #[test]
    fn failed_outcome_does_not_write_to_store() {
        let store = SeenSpyStore::default();
        let v = verified("failed-id");
        let result = ActionResult::Failed("entry-failed: broker 500".into());
        run(record_dispatcher_outcome(&store, &v, now(), &result));
        assert!(
            store.marks().is_empty(),
            "Failed must not write to seen — next fire is allowed to retry",
        );
    }

    /// Walk every gate-rejection outcome string the worker emits today
    /// and assert each classifies as `Skip`. Strings here correspond
    /// to real rejection sites in `run_enter` / `run_close` /
    /// `run_invalidate` / the retry gate, taken from the Phase 1
    /// exploration of `ActionResult::Rejected` call sites.
    ///
    /// The CHF/JPY 2026-06-02 incident bottomed out at fire 4 with
    /// `"rejected: missing-prep (break-and-close)"` — that's the
    /// first case below. Every other transient or terminal rejection
    /// gets the same treatment: log and move on, do not poison the id.
    ///
    /// Note this tests via [`seen_decision`] rather than the full
    /// async helper: constructing `ActionResult::Rejected` requires a
    /// `worker::Result<worker::Response>` in the `response` field,
    /// which calls into wasm-bindgen at construction and panics
    /// off-wasm. The pure decision rule is what we care about; the
    /// async helper just turns `SeenDecision::Mark` into a
    /// `store.mark_seen` call.
    ///
    /// To synthesize the `Rejected` variant safely we'd need to
    /// either fake a `worker::Response` (not possible — it's a
    /// wasm-bindgen wrapper) or test through a public API surface
    /// that constructs them naturally. Neither pays off enough to
    /// justify the complexity. The match in `seen_decision` is
    /// trivially auditable.
    #[test]
    fn every_rejection_outcome_classifies_as_skip() {
        let cases = [
            "rejected: missing-prep (break-and-close)",
            "rejected: prep-order-violated (retest)",
            "rejected: veto-active (too-high)",
            "rejected: cooled-down",
            "rejected: paused [news-window]",
            "rejected: allow-entry-false",
            "rejected: needs-golden",
            "rejected: allow-entry-eval",
            "rejected: resolve-failed",
            "rejected: state-error",
            "rejected: retry-cap (5)",
            "rejected: retry-fire-replay",
            "rejected: trade-already-open",
            "rejected: broker-transient",
            "rejected: max-retries-zero",
            "rejected: missing-trade-id",
            "rejected: price-fetch-failed",
            "rejected: expiry-bars-out-of-range",
            "rejected: expiry-bars-script-parse",
            "rejected: market-blackout",
        ];
        // Use Failed as the carrier — the decision rule treats
        // Failed and Rejected identically (both Skip), and Failed is
        // wasm-safe to construct off-wasm.
        for outcome in cases {
            let result = ActionResult::Failed(outcome.into());
            assert!(
                matches!(seen_decision(&result), SeenDecision::Skip { .. }),
                "outcome {outcome:?} unexpectedly classified as Mark \
                 — non-Ok outcomes must Skip the seen index",
            );
        }
    }

    /// Control actions (`prep`, `veto`, `pause`, `clear-*`, `status`,
    /// `news-*`, `unlock`) use a separate [`record_seen`] helper and
    /// **do** mark seen on completion. That's legitimate idempotency
    /// for state-set ops — a replayed `prep` message should not
    /// double-refresh its TTL. This regression test pins that
    /// behaviour so a future "blanket-strip mark_seen writes"
    /// refactor can't silently break it.
    #[test]
    fn control_action_record_seen_still_marks() {
        let store = SeenSpyStore::default();
        let mut v = verified("prep-msg-id");
        v.intent.action = Action::Prep;
        v.intent.step = Some("break-and-close".into());
        run(record_seen(
            &store,
            &v,
            now(),
            "prep-set: break-and-close ttl=24h",
        ));
        assert_eq!(
            store.marks(),
            vec![(
                "prep-msg-id".into(),
                "prep-set: break-and-close ttl=24h".into(),
            )],
            "Control-action record_seen must still mark seen — replay protection \
             on state-set ops (prep/veto/pause/etc) is legitimate idempotency",
        );
    }

    // ---- is_multishot_enter: the top-level replay-guard exemption ----

    /// An `enter` that opted into `max_retries` and carries a `trade_id`
    /// is the case the bug fix targets: its baked-in id re-fires every
    /// bar, so the top-level `is_seen` 409 must defer to the retry gate.
    #[test]
    fn multishot_enter_is_detected() {
        let mut v = verified("ent-id");
        v.intent.action = Action::Enter;
        v.intent.max_retries = Tunable::Static(3);
        v.intent.trade_id = Some("trade-xyz".into());
        assert!(is_multishot_enter(&v.intent));
    }

    /// Single-shot enter (`max_retries` default `Static(0)`) keeps the
    /// byte-identical top-level 409 — the retry gate never runs for it.
    #[test]
    fn single_shot_enter_is_not_multishot() {
        let mut v = verified("ent-id");
        v.intent.action = Action::Enter;
        v.intent.max_retries = Tunable::Static(0);
        v.intent.trade_id = Some("trade-xyz".into());
        assert!(!is_multishot_enter(&v.intent));
    }

    /// Without a `trade_id` the retry gate does no per-bar dedup, so the
    /// intent must stay on the top-level 409 path even with `max_retries`.
    #[test]
    fn multishot_enter_without_trade_id_is_not_multishot() {
        let mut v = verified("ent-id");
        v.intent.action = Action::Enter;
        v.intent.max_retries = Tunable::Static(3);
        v.intent.trade_id = None;
        assert!(!is_multishot_enter(&v.intent));
    }

    /// Any non-`Static(0)` Tunable (including a script) counts as
    /// multi-shot — mirrors the `max_retries != Static(0)` test the
    /// run_enter gate uses.
    #[test]
    fn enter_with_script_max_retries_is_multishot() {
        let mut v = verified("ent-id");
        v.intent.action = Action::Enter;
        v.intent.max_retries = Tunable::from_script("3");
        v.intent.trade_id = Some("trade-xyz".into());
        assert!(is_multishot_enter(&v.intent));
    }

    /// Only `Enter` is exempted. A control action that happens to carry a
    /// stray `max_retries` + `trade_id` still 409s at the top level.
    #[test]
    fn control_actions_are_not_multishot() {
        for action in [
            Action::Close,
            Action::Invalidate,
            Action::Veto,
            Action::Prep,
            Action::Status,
            Action::Pause,
        ] {
            let mut v = verified("ctl-id");
            v.intent.action = action;
            v.intent.max_retries = Tunable::Static(3);
            v.intent.trade_id = Some("trade-xyz".into());
            assert!(
                !is_multishot_enter(&v.intent),
                "{action:?} must not be treated as a multi-shot enter",
            );
        }
    }
}

#[cfg(test)]
mod plan_show_tests {
    //! Pins [`collect_plan_details`]: `plan show` must find a plan whether it
    //! is a live registered plan **or** an archived (terminated) one. The bug
    //! that motivated this: a finished plan surfaced by `plan list
    //! --include-archived` 404'd on `plan show`, because the handler only
    //! scanned live plans.
    use super::*;
    use chrono::{TimeZone, Utc};
    use trade_control_core::plan_state::{Phase, PlanState};
    use trade_control_core::state::{MemStateStore, StateStore};
    use trade_control_core::trade_plan::TradePlan;

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
