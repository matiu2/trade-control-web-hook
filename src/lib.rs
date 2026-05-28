mod accounts;
mod admin;
mod allow_entry_gate;
mod cron;
mod diag;
mod retry_gate;
mod state;
#[cfg(target_arch = "wasm32")]
mod tn_login;
mod tn_login_helpers;
mod tracing_console;
mod tradenation_adapter;

use chrono::Utc;
use worker::{Context, Env, Method, Request, Response, Result, console_error, console_log, event};

use crate::state::KvStateStore;
use crate::tradenation_adapter::TradeNationAdapter;
#[cfg(target_arch = "wasm32")]
use broker_oanda::login_with_account_id as oanda_login_with;
use broker_oanda::{OandaBroker, login as oanda_login};
use serde::Serialize;
use trade_control_core::broker::{Broker, EntryRequest};
use trade_control_core::incoming::{self, parse_and_verify};
use trade_control_core::intent::{Action, BrokerKind, Resolved, Shell, VetoLevel};
use trade_control_core::rules::{self, RuleError};
use trade_control_core::sig;
use trade_control_core::state::{
    StateStore, clear_named_preps, clear_named_vetos, veto_ttl_seconds,
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
pub async fn main(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    tracing_console::ConsoleSubscriber::install();

    // Diagnostic routes — GET-only, gated by X-Diag-Key. Handled before
    // we consume the body so the body parser doesn't apply.
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
    // POST) don't follow the signed-body shape.
    let path = req.path();
    if path.starts_with("/admin/") {
        return route_admin(&mut req, &env, &path).await;
    }

    let yaml = req.text().await?;

    let key_hex = match get_secret(SIGNING_KEY_SECRET, &env) {
        Some(s) => s,
        None => {
            console_error!("missing required secret: {SIGNING_KEY_SECRET}");
            return Response::error("server misconfigured", 500);
        }
    };
    let key = match sig::parse_key_hex(&key_hex) {
        Ok(k) => k,
        Err(err) => {
            console_error!("SIGNING_KEY is not valid hex: {err}");
            return Response::error("server misconfigured", 500);
        }
    };

    let now = Utc::now();
    let verified = match parse_and_verify(&yaml, &key, now) {
        Ok(v) => v,
        Err(err) => {
            console_error!(
                "incoming rejected: {err} | body_len={} body_excerpt={:?}",
                yaml.len(),
                body_excerpt(&yaml)
            );
            return Response::error("rejected", 400);
        }
    };

    let store = match env.kv(KV_NAMESPACE) {
        Ok(kv) => KvStateStore::new(kv),
        Err(err) => {
            console_error!("missing KV namespace {KV_NAMESPACE}: {err:?}");
            return Response::error("server misconfigured", 500);
        }
    };

    // Replay protection.
    match store.is_seen(&verified.intent.id).await {
        Ok(true) => return Response::error("replay", 409),
        Ok(false) => {}
        Err(err) => {
            console_error!("KV is_seen: {err}");
            return Response::error("state error", 500);
        }
    }

    // Control actions don't touch the broker; handle them up front so we
    // don't waste a broker login on them. The exception is a `veto` with
    // `level` above `stop-next-entry` — those cancel pending orders or
    // close positions, which need broker auth, so they fall through to
    // the broker dispatch below.
    match verified.intent.action {
        Action::Status => return handle_status(&store, &verified, now).await,
        Action::Unlock => return handle_unlock(&store, &verified, now).await,
        Action::Prep => return handle_prep(&store, &verified, now).await,
        Action::Veto => {
            if matches!(
                verified.intent.level.unwrap_or_default(),
                VetoLevel::StopNextEntry
            ) {
                return handle_veto(&store, &verified, now).await;
            }
            // Higher-level vetos need the broker; fall through.
        }
        Action::ClearPrep => return handle_clear_prep(&store, &verified, now).await,
        Action::ClearVeto => return handle_clear_veto(&store, &verified, now).await,
        Action::Pause => return handle_pause(&store, &verified, now).await,
        Action::Resume => return handle_resume(&store, &verified, now).await,
        Action::NewsStart => return handle_news_start(&store, &verified, now).await,
        Action::NewsEnd => return handle_news_end(&store, &verified, now).await,
        _ => {}
    }

    // Broker dispatch.
    let result = match verified.intent.broker {
        BrokerKind::Oanda => {
            match acquire_oanda_broker(&env, verified.intent.account.as_deref()).await {
                Some(broker) => run_action(&broker, &store, &verified, &env, now).await,
                None => return Response::error("oanda login failed", 500),
            }
        }
        BrokerKind::TradeNation => {
            match acquire_tn_broker(&env, verified.intent.account.as_deref()).await {
                Some(broker) => {
                    let adapter = TradeNationAdapter(broker);
                    run_action(&adapter, &store, &verified, &env, now).await
                }
                None => {
                    return Response::error(
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
    let (response, outcome) = match result {
        ActionResult::Ok(outcome) => (Response::ok("ok"), outcome),
        ActionResult::Failed(outcome) => (Response::error("action failed", 502), outcome),
        ActionResult::Rejected { response, outcome } => {
            return mark_seen_and_return(&store, &verified, now, &outcome, response).await;
        }
    };
    let ttl = incoming::replay_ttl_seconds(verified.intent.not_after, now);
    if let Err(err) = store
        .mark_seen(
            &verified.intent.id,
            verified.intent.action,
            now,
            &outcome,
            ttl,
            verified.intent.trade_id.as_deref(),
        )
        .await
    {
        console_error!("KV mark_seen after action: {err}");
    }
    response
}

/// Dispatch on `/admin/...` paths. Method + path together select the
/// right admin handler. Path parsing is deliberately concrete here
/// rather than a router crate — we have 4 routes and one path-segment
/// parameter (`<name>`), and the explicit `match` is easier to audit.
async fn route_admin(req: &mut Request, env: &Env, path: &str) -> Result<Response> {
    // POST /admin/accounts                      — add (body)
    // DELETE /admin/accounts/<name>             — remove
    // POST   /admin/accounts/<name>/test        — verify creds
    let method = req.method();
    if method == Method::Post && path == "/admin/accounts" {
        return admin::handle_add(req, env).await;
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

/// Helper: record an outcome on the seen index and return the caller's
/// response. Used by every early-return path so `status` always reflects
/// what happened to an id.
async fn mark_seen_and_return(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
    outcome: &str,
    response: Result<Response>,
) -> Result<Response> {
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
        console_error!("KV mark_seen on early return: {err}");
    }
    response
}

/// Outcome of an action dispatch. Every variant carries a short
/// human-readable `outcome` string that lands in the `seen` index so
/// `status` can answer "what did this id do?".
enum ActionResult {
    /// Action completed successfully. The outcome (e.g. `"entered"`) is
    /// recorded against the seen id.
    Ok(String),
    /// Action reached the broker but the broker call failed. Recorded
    /// against the seen id; HTTP response is 502.
    Failed(String),
    /// Action was rejected before reaching the broker (gate, validation,
    /// state error). The `response` is returned to the caller and the
    /// `outcome` is recorded against the seen id.
    Rejected {
        response: Result<Response>,
        outcome: String,
    },
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
) -> ActionResult {
    match verified.intent.action {
        Action::Enter => run_enter(broker, store, verified, env, now).await,
        Action::Close => run_close(broker, store, verified).await,
        Action::Invalidate => {
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
                console_error!("KV set_cooldown: {err}");
                return ActionResult::Rejected {
                    response: Response::error("state error", 500),
                    outcome: "rejected: state-error".into(),
                };
            }
            let cancelled = broker
                .cancel_pending_for_instrument(&verified.intent.instrument)
                .await;
            console_log!(
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
        Action::Veto => run_veto_with_broker(broker, store, verified, now).await,
        Action::Status
        | Action::Unlock
        | Action::Prep
        | Action::ClearPrep
        | Action::ClearVeto
        | Action::Pause
        | Action::Resume
        | Action::NewsStart
        | Action::NewsEnd => {
            // Handled before broker dispatch; never reached here.
            unreachable!("non-broker actions handled before broker dispatch")
        }
    }
}

/// Dispatch a `Close` intent. When `require_news_window: true`, gates
/// the close on an active news window for the trade — used by the
/// opposing-direction golden-reversal alert so that the same alert
/// outside news is ignored. Without that flag the close is
/// unconditional (operator emergency-close path).
async fn run_close<B: Broker>(
    broker: &B,
    store: &KvStateStore,
    verified: &incoming::Verified,
) -> ActionResult {
    if matches!(verified.intent.require_news_window, Some(true)) {
        let Some(tid) = verified.intent.trade_id.as_deref() else {
            return ActionResult::Rejected {
                response: Response::error(
                    "close with require_news_window requires `trade_id`",
                    400,
                ),
                outcome: "rejected: missing-trade-id".into(),
            };
        };
        match store.list_news_windows_for_trade(tid).await {
            Ok(windows) if windows.is_empty() => {
                console_log!(
                    "close rejected: trade {tid} has no active news window (require_news_window: true)"
                );
                return ActionResult::Rejected {
                    response: Response::error("no news window active", 423),
                    outcome: "rejected: no-news-window".into(),
                };
            }
            Ok(windows) => {
                let names: Vec<String> = windows
                    .iter()
                    .map(|w| match &w.reason {
                        Some(r) => format!("{}({r})", w.news_id),
                        None => w.news_id.clone(),
                    })
                    .collect();
                console_log!(
                    "close gated by news window: trade {tid} active=[{}]",
                    names.join(", ")
                );
            }
            Err(err) => {
                console_error!("KV list_news_windows_for_trade: {err}");
                return ActionResult::Rejected {
                    response: Response::error("state error", 500),
                    outcome: "rejected: state-error".into(),
                };
            }
        }
    }
    let ok = broker.close_positions(&verified.intent.instrument).await;
    if ok {
        ActionResult::Ok("closed".into())
    } else {
        ActionResult::Failed("close-failed".into())
    }
}

async fn run_enter<B: Broker>(
    broker: &B,
    store: &KvStateStore,
    verified: &incoming::Verified,
    env: &Env,
    now: chrono::DateTime<chrono::Utc>,
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
                console_log!(
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
                console_error!("KV list_pauses_for_trade: {err}");
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
    // earlier attempt is still open) and enforces the placement cap.
    // The single-shot path (`max_retries: Static(0)`, the default)
    // skips this branch entirely so no new KV/broker calls land on the
    // byte-identical baseline.
    let retry_attempt_no = if !matches!(
        verified.intent.max_retries,
        trade_control_core::tunable::Tunable::Static(0)
    ) {
        match retry_gate::evaluate(broker, store, &verified.intent, &verified.shell).await {
            retry_gate::RetryGateOutcome::Proceed { next_attempt_no } => Some(next_attempt_no),
            retry_gate::RetryGateOutcome::Rejected {
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
            console_log!(
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
            console_error!("KV is_cooled_down: {err}");
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
                    console_log!(
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
                console_log!(
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
                console_error!("KV get_prep: {err}");
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
    // `account:`) still blocks every account by design.
    for veto in &verified.intent.vetos {
        match store
            .is_vetoed(
                verified.intent.account.as_deref(),
                &verified.intent.instrument,
                veto,
            )
            .await
        {
            Ok(true) => {
                console_log!(
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
                console_error!("KV is_vetoed: {err}");
                return ActionResult::Rejected {
                    response: Response::error("state error", 500),
                    outcome: "rejected: state-error".into(),
                };
            }
        }
    }

    let worker_max_risk_pct = secret_or_default(env, MAX_RISK_PCT_PER_TRADE_SECRET, 1.0);
    let worker_max_open_positions = secret_or_default(env, MAX_OPEN_POSITIONS_SECRET, 3.0) as u32;
    let pip_size = pip_size_for(env, &verified.intent.instrument);

    let resolved = match Resolved::from_intent(&verified.intent, &verified.shell, pip_size) {
        Ok(r) => r,
        Err(err) => {
            console_error!("resolve: {err}");
            return ActionResult::Rejected {
                response: Response::error("rejected", 400),
                outcome: "rejected: resolve-failed".into(),
            };
        }
    };

    // allow_entry gate — operator's Tunable<bool> script sees the full
    // shell + resolved geometry. Sits after Resolved::from_intent
    // (Phase 2 bindings need it) and ahead of the broker call (cheap
    // 412 on false). Doesn't consume a retry slot — only a successful
    // broker placement does.
    match allow_entry_gate::evaluate(&verified.intent, &verified.shell, &resolved, pip_size) {
        allow_entry_gate::AllowEntryOutcome::Proceed => {}
        allow_entry_gate::AllowEntryOutcome::Blocked => {
            console_log!(
                "entry rejected: allow_entry returned false (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                response: Response::error("entry blocked", 412),
                outcome: "rejected: allow-entry-false".into(),
            };
        }
        allow_entry_gate::AllowEntryOutcome::NeedsGoldenUnmet => {
            console_log!(
                "entry rejected: needs_golden set but shell.golden != Some(true) (id={})",
                verified.intent.id
            );
            return ActionResult::Rejected {
                response: Response::error("entry blocked: needs-golden", 412),
                outcome: "rejected: needs-golden".into(),
            };
        }
        allow_entry_gate::AllowEntryOutcome::ScriptError { kind, message } => {
            console_error!(
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
    console_log!(
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

    match broker
        .place_entry(max_risk_pct, max_open_positions, &entry_request)
        .await
    {
        Ok(order_id) => {
            if resolved.dry_run {
                console_log!("DRY-RUN entry id={} (not placed)", verified.intent.id);
                ActionResult::Ok(format!("dry-run: id={}", verified.intent.id))
            } else {
                console_log!("entry placed id={} order={}", verified.intent.id, order_id);
                if let Some(attempt_no) = retry_attempt_no {
                    retry_gate::record_placement(
                        store,
                        &verified.intent,
                        verified.shell.time,
                        verified.intent.not_after,
                        now,
                        attempt_no,
                        &order_id,
                        resolved.direction,
                        resolved.stop_loss,
                    )
                    .await;
                }
                ActionResult::Ok(format!("entered: order={order_id}"))
            }
        }
        Err(err) => {
            console_error!("entry failed: {err}");
            ActionResult::Failed(format!("entry-failed: {err}"))
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
            console_error!("KV snapshot: {err}");
            return Response::error("state error", 500);
        }
    };
    let body = match serde_yaml::to_string(&snap) {
        Ok(s) => s,
        Err(err) => {
            console_error!("snapshot serialise: {err}");
            return Response::error("internal error", 500);
        }
    };
    record_seen(store, verified, now, "status").await;
    Response::ok(body)
}

/// Best-effort wrapper around `mark_seen`. Used by the dedicated control
/// handlers (status / unlock / prep / veto / clear-*) so each one ends
/// with one line instead of an `if let Err` repeated everywhere.
async fn record_seen(
    store: &KvStateStore,
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
        console_error!("KV mark_seen ({outcome}): {err}");
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
            console_error!("KV clear_cooldown: {err}");
            return Response::error("state error", 500);
        }
    };
    console_log!(
        "unlock instrument={instrument} account={} was_cooled_down={was}",
        account.unwrap_or("<global>")
    );
    let body = match serde_yaml::to_string(&UnlockResponse {
        unlocked: instrument.clone(),
        was_cooled_down: was,
    }) {
        Ok(s) => s,
        Err(err) => {
            console_error!("unlock serialise: {err}");
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
    // Clear any preps listed in `clears` first so stale downstream
    // preps (e.g. an old `retest`) can't survive a fresh upstream prep
    // (`break-and-close`). Logged per-name for traceability; failures
    // are best-effort logs rather than rejections so a transient KV
    // hiccup on a clear doesn't block the new prep.
    let account = verified.intent.account.as_deref();
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
            console_error!("KV clear_named_preps (in clears): {err}");
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
        console_error!("KV set_prep: {err}");
        return Response::error("state error", 500);
    }
    console_log!(
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
    // this intent's account, same as the `set_veto` that follows.
    let account = verified.intent.account.as_deref();
    let cleared = match clear_named_vetos(
        store,
        account,
        &verified.intent.instrument,
        &verified.intent.clears,
    )
    .await
    {
        Ok(c) => c,
        Err(err) => {
            console_error!("KV clear_named_vetos (in clears): {err}");
            Vec::new()
        }
    };
    if let Err(err) = store
        .set_veto(account, &verified.intent.instrument, name, ttl_seconds)
        .await
    {
        console_error!("KV set_veto: {err}");
        return Response::error("state error", 500);
    }
    console_log!(
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
    let cleared = match clear_named_vetos(store, account, instrument, &verified.intent.clears).await
    {
        Ok(c) => c,
        Err(err) => {
            console_error!("KV clear_named_vetos (in clears): {err}");
            Vec::new()
        }
    };
    if let Err(err) = store.set_veto(account, instrument, name, ttl_seconds).await {
        console_error!("KV set_veto: {err}");
        return ActionResult::Rejected {
            response: Response::error("state error", 500),
            outcome: "rejected: state-error".into(),
        };
    }

    let cancelled = broker.cancel_pending_for_instrument(instrument).await;
    let closed_ok = match level {
        VetoLevel::ClosePositions => broker.close_positions(instrument).await,
        // No close requested at this level.
        VetoLevel::CancelPending | VetoLevel::StopNextEntry => true,
    };

    console_log!(
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
            console_error!("KV clear_prep: {err}");
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
        console_error!("KV forget_seen({setter_id}): {err}");
    }
    let was = cleared_setter.is_some();
    console_log!(
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
    let account = verified.intent.account.as_deref();
    let was = match store
        .clear_veto(account, &verified.intent.instrument, name)
        .await
    {
        Ok(b) => b,
        Err(err) => {
            console_error!("KV clear_veto: {err}");
            return Response::error("state error", 500);
        }
    };
    console_log!(
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
        console_error!("KV set_pause: {err}");
        return Response::error("state error", 500);
    }
    console_log!(
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
            console_error!("KV clear_pause: {err}");
            return Response::error("state error", 500);
        }
    };
    console_log!("resume: trade_id={trade_id} blackout_id={blackout_id} was_set={was}");
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
        console_error!("KV set_news_window: {err}");
        return Response::error("state error", 500);
    }
    console_log!(
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
            console_error!("KV clear_news_window: {err}");
            return Response::error("state error", 500);
        }
    };
    console_log!("news-end: trade_id={trade_id} news_id={news_id} was_set={was}");
    let outcome = if was {
        format!("news-end: {news_id}")
    } else {
        format!("news-end: {news_id} (noop)")
    };
    record_seen(store, verified, now, &outcome).await;
    Response::ok("ok")
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
            console_error!("oanda[{name}]: KV binding missing: {err:?}");
            return None;
        }
    };
    let metadata = accounts::KvMetadataStore::new(kv);
    let meta = match metadata.get(name).await {
        Ok(m) => m,
        Err(err) => {
            console_error!("oanda[{name}]: metadata lookup failed: {err}");
            return None;
        }
    };
    if meta.broker != BrokerKind::Oanda {
        console_error!(
            "oanda[{name}]: account broker={:?} but intent routed to oanda",
            meta.broker
        );
        return None;
    }
    match meta.oanda_account_id {
        Some(id) => oanda_login_with(env, id).await,
        None => {
            console_error!(
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
/// is logged at `console_error!`. Intents without `account:` are
/// rejected at the caller; the legacy shared-session paths are gone.
pub(crate) async fn acquire_tn_broker(
    env: &Env,
    account: Option<&str>,
) -> Option<broker_tradenation::TradeNationBroker> {
    match account {
        Some(name) => acquire_tn_broker_for_account(env, name).await,
        None => {
            console_error!(
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
            console_error!("caps[{name}]: KV binding missing: {err:?}");
            return AccountCaps::default();
        }
    };
    let metadata = accounts::KvMetadataStore::new(kv);
    match metadata.get(name).await {
        Ok(m) => m.caps,
        Err(err) => {
            console_error!("caps[{name}]: metadata lookup failed: {err}");
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
            console_error!("tn[{name}]: KV binding missing: {err:?}");
            return None;
        }
    };
    let metadata = accounts::KvMetadataStore::new(kv.clone());
    let meta = match metadata.get(name).await {
        Ok(m) => m,
        Err(err) => {
            console_error!("tn[{name}]: metadata lookup failed: {err}");
            return None;
        }
    };
    if meta.broker != BrokerKind::TradeNation {
        console_error!(
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
                console_log!("tn[{name}]: using cached session");
                return Some(broker);
            }
            console_log!("tn[{name}]: cached session rejected, will re-login");
        }
        Ok(None) => console_log!("tn[{name}]: no cached session, will login"),
        Err(err) => console_error!("tn[{name}]: KV get session: {err:?}"),
    }

    // 2. Resolve credentials.
    let resolver = accounts::SecretCredentialsResolver::new(env, &metadata);
    let creds = match resolver.resolve(name).await {
        Ok(c) => c,
        Err(err) => {
            console_error!("tn[{name}]: credentials resolve: {err}");
            return None;
        }
    };
    let tn_creds = match creds {
        Credentials::TradeNation(c) => c,
        Credentials::Oanda(_) => {
            console_error!("tn[{name}]: credential payload is OANDA — broker mismatch");
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
            console_error!("tn[{account_name}]: demo login failed: {err}");
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
            console_error!("tn[{account_name}]: live login failed: {err}");
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
            console_error!("tn[{account_name}]: serialise session: {err}");
            return None;
        }
    };
    if let Ok(kv) = env.kv(KV_NAMESPACE) {
        match kv.put(cache_key, json.clone()) {
            Ok(builder) => {
                if let Err(err) = builder.execute().await {
                    console_error!("tn[{account_name}]: KV put session execute: {err:?}");
                }
            }
            Err(err) => console_error!("tn[{account_name}]: KV put session builder: {err:?}"),
        }
        write_session_meta(&kv, account_name).await;
    }
    if let Some(broker) = broker_tradenation::login(&json).await {
        console_log!("tn[{account_name}]: fresh {kind_label} login");
        return Some(broker);
    }
    console_error!("tn[{account_name}]: fresh session rejected by broker_tradenation::login");
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
            console_error!("tn[{account_name}]: serialise session_meta: {err}");
            return;
        }
    };
    let key = cron::session_meta::key(account_name);
    match kv.put(&key, json) {
        Ok(builder) => {
            if let Err(err) = builder.execute().await {
                console_error!("tn[{account_name}]: KV put session_meta execute: {err:?}");
            }
        }
        Err(err) => console_error!("tn[{account_name}]: KV put session_meta builder: {err:?}"),
    }
}

/// Read a secret. Returns `None` if the binding is absent or unreadable.
/// Silent on absence — callers decide whether a miss is an error worth logging.
pub(crate) fn get_secret(name: &str, env: &Env) -> Option<String> {
    env.secret(name).map(|value| value.to_string()).ok()
}

/// Read a numeric secret, falling back to `default` if missing or unparsable.
fn secret_or_default(env: &Env, name: &str, default: f64) -> f64 {
    get_secret(name, env)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Pip size for an instrument. Override via `PIP_SIZE_<INSTRUMENT>` secret
/// (e.g. `PIP_SIZE_USD_JPY=0.01`). Indices like SPX500_USD also need overrides.
fn pip_size_for(env: &Env, instrument: &str) -> f64 {
    let key = format!("{PIP_SIZE_SECRET_PREFIX}{instrument}");
    get_secret(&key, env)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PIP_SIZE)
}
