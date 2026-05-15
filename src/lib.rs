mod state;
mod tradenation_adapter;

use chrono::Utc;
use worker::{Context, Env, Request, Response, Result, console_error, console_log, event};

use crate::state::KvStateStore;
use crate::tradenation_adapter::TradeNationAdapter;
use broker_oanda::login as oanda_login;
use serde::Serialize;
use trade_control_core::broker::{Broker, EntryRequest};
use trade_control_core::crypto;
use trade_control_core::incoming::{self, parse_and_verify};
use trade_control_core::intent::{Action, BrokerKind, Resolved};
use trade_control_core::state::StateStore;

/// Response body for the `unlock` action. Serialised as YAML.
#[derive(Serialize)]
struct UnlockResponse {
    unlocked: String,
    was_cooled_down: bool,
}

const ENCRYPTION_KEY_SECRET: &str = "ENCRYPTION_KEY";
const MAX_RISK_PCT_PER_TRADE_SECRET: &str = "MAX_RISK_PCT_PER_TRADE";
const MAX_OPEN_POSITIONS_SECRET: &str = "MAX_OPEN_POSITIONS";
const PIP_SIZE_SECRET_PREFIX: &str = "PIP_SIZE_";
const TN_SESSION_JSON_SECRET: &str = "TN_SESSION_JSON";
const KV_NAMESPACE: &str = "TRADE_CONTROL_KV";

/// Default pip size when no `PIP_SIZE_<INSTRUMENT>` secret is set. EUR_USD's
/// pip size; works for most majors. JPY pairs / indices need an override.
const DEFAULT_PIP_SIZE: f64 = 0.0001;

#[event(fetch)]
pub async fn main(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let yaml = req.text().await?;

    let key_hex = match get_secret(ENCRYPTION_KEY_SECRET, &env) {
        Some(s) => s,
        None => {
            console_error!("missing required secret: {ENCRYPTION_KEY_SECRET}");
            return Response::error("server misconfigured", 500);
        }
    };
    let key = match crypto::parse_key_hex(&key_hex) {
        Ok(k) => k,
        Err(err) => {
            console_error!("ENCRYPTION_KEY is not valid hex: {err}");
            return Response::error("server misconfigured", 500);
        }
    };

    let now = Utc::now();
    let verified = match parse_and_verify(&yaml, &key, now) {
        Ok(v) => v,
        Err(err) => {
            console_error!("incoming rejected: {err}");
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
    // don't waste a broker login on them.
    match verified.intent.action {
        Action::Status => return handle_status(&store, &verified, now).await,
        Action::Unlock => return handle_unlock(&store, &verified, now).await,
        Action::Prep => return handle_prep(&store, &verified, now).await,
        Action::Veto => return handle_veto(&store, &verified, now).await,
        Action::ClearPrep => return handle_clear_prep(&store, &verified, now).await,
        Action::ClearVeto => return handle_clear_veto(&store, &verified, now).await,
        _ => {}
    }

    // Broker dispatch.
    let result = match verified.intent.broker {
        BrokerKind::Oanda => match oanda_login(&env).await {
            Some(broker) => run_action(&broker, &store, &verified, &env, now).await,
            None => return Response::error("oanda login failed", 500),
        },
        BrokerKind::TradeNation => {
            let Some(session_json) = get_secret(TN_SESSION_JSON_SECRET, &env) else {
                console_error!("missing required secret: {TN_SESSION_JSON_SECRET}");
                return Response::error("tradenation session not configured", 503);
            };
            match broker_tradenation::login(&session_json).await {
                Some(broker) => {
                    let adapter = TradeNationAdapter(broker);
                    run_action(&adapter, &store, &verified, &env, now).await
                }
                None => {
                    console_error!("tradenation login failed; rotate {TN_SESSION_JSON_SECRET}");
                    return Response::error("tradenation session expired or invalid", 503);
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
        )
        .await
    {
        console_error!("KV mark_seen after action: {err}");
    }
    response
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

/// Dispatch `Enter` / `Close` / `Invalidate` against an authenticated broker.
/// Status / Unlock are handled before this function and never reach it.
async fn run_action<B: Broker>(
    broker: &B,
    store: &KvStateStore,
    verified: &incoming::Verified,
    env: &Env,
    now: chrono::DateTime<chrono::Utc>,
) -> ActionResult {
    match verified.intent.action {
        Action::Enter => run_enter(broker, store, verified, env, now).await,
        Action::Close => {
            let ok = broker.close_positions(&verified.intent.instrument).await;
            if ok {
                ActionResult::Ok("closed".into())
            } else {
                ActionResult::Failed("close-failed".into())
            }
        }
        Action::Invalidate => {
            let hours = verified.intent.cooldown_hours.unwrap_or(12);
            if let Err(err) = store
                .set_cooldown(&verified.intent.instrument, hours, now)
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
                "invalidate {} cooldown {}h cancelled {} pending",
                verified.intent.instrument,
                hours,
                cancelled
            );
            ActionResult::Ok(format!(
                "invalidated: cooldown {hours}h, cancelled {cancelled}"
            ))
        }
        Action::Status
        | Action::Unlock
        | Action::Prep
        | Action::Veto
        | Action::ClearPrep
        | Action::ClearVeto => {
            // Handled before broker dispatch; never reached here.
            unreachable!("non-broker actions handled before broker dispatch")
        }
    }
}

async fn run_enter<B: Broker>(
    broker: &B,
    store: &KvStateStore,
    verified: &incoming::Verified,
    env: &Env,
    _now: chrono::DateTime<chrono::Utc>,
) -> ActionResult {
    // Cooldown gate
    match store.is_cooled_down(&verified.intent.instrument).await {
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
        match store.get_prep(&verified.intent.instrument, step).await {
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
    for veto in &verified.intent.vetos {
        match store.is_vetoed(&verified.intent.instrument, veto).await {
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

    let max_risk_pct = secret_or_default(env, MAX_RISK_PCT_PER_TRADE_SECRET, 1.0);
    let max_open_positions = secret_or_default(env, MAX_OPEN_POSITIONS_SECRET, 3.0) as u32;
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
    let entry_request = EntryRequest {
        instrument: &resolved.instrument,
        direction: resolved.direction,
        entry: resolved.entry.clone(),
        stop_loss: resolved.stop_loss,
        take_profit: resolved.take_profit,
        risk_pct: resolved.risk_pct,
    };
    match broker
        .place_entry(max_risk_pct, max_open_positions, &entry_request)
        .await
    {
        Ok(order_id) => {
            console_log!("entry placed id={} order={}", verified.intent.id, order_id);
            ActionResult::Ok(format!("entered: order={order_id}"))
        }
        Err(err) => {
            console_error!("entry failed: {err}");
            ActionResult::Failed(format!("entry-failed: {err}"))
        }
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
    let was = match store.clear_cooldown(instrument).await {
        Ok(b) => b,
        Err(err) => {
            console_error!("KV clear_cooldown: {err}");
            return Response::error("state error", 500);
        }
    };
    console_log!("unlock {instrument} was_cooled_down={was}");
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
    let Some(ttl_hours) = verified.intent.ttl_hours else {
        return Response::error("prep requires `ttl_hours`", 400);
    };
    let ttl_seconds = (ttl_hours as u64).saturating_mul(3600);
    if let Err(err) = store
        .set_prep(&verified.intent.instrument, step, now, ttl_seconds)
        .await
    {
        console_error!("KV set_prep: {err}");
        return Response::error("state error", 500);
    }
    console_log!(
        "prep set: {} {} ttl={}h",
        verified.intent.instrument,
        step,
        ttl_hours
    );
    let outcome = format!("prep-set: {step} ttl={ttl_hours}h");
    record_seen(store, verified, now, &outcome).await;
    Response::ok("ok")
}

/// Handle the `veto` action: record a named veto for an instrument with a TTL.
async fn handle_veto(
    store: &KvStateStore,
    verified: &incoming::Verified,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Response> {
    let Some(name) = verified.intent.name.as_deref() else {
        return Response::error("veto requires `name`", 400);
    };
    let Some(ttl_hours) = verified.intent.ttl_hours else {
        return Response::error("veto requires `ttl_hours`", 400);
    };
    let ttl_seconds = (ttl_hours as u64).saturating_mul(3600);
    if let Err(err) = store
        .set_veto(&verified.intent.instrument, name, ttl_seconds)
        .await
    {
        console_error!("KV set_veto: {err}");
        return Response::error("state error", 500);
    }
    console_log!(
        "veto set: {} {} ttl={}h",
        verified.intent.instrument,
        name,
        ttl_hours
    );
    let outcome = format!("veto-set: {name} ttl={ttl_hours}h");
    record_seen(store, verified, now, &outcome).await;
    Response::ok("ok")
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
    let was = match store.clear_prep(&verified.intent.instrument, step).await {
        Ok(b) => b,
        Err(err) => {
            console_error!("KV clear_prep: {err}");
            return Response::error("state error", 500);
        }
    };
    console_log!(
        "clear-prep {} {} was_set={}",
        verified.intent.instrument,
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
    let was = match store.clear_veto(&verified.intent.instrument, name).await {
        Ok(b) => b,
        Err(err) => {
            console_error!("KV clear_veto: {err}");
            return Response::error("state error", 500);
        }
    };
    console_log!(
        "clear-veto {} {} was_set={}",
        verified.intent.instrument,
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

/// Read a secret. Returns `None` if the binding is absent or unreadable.
/// Silent on absence — callers decide whether a miss is an error worth logging.
fn get_secret(name: &str, env: &Env) -> Option<String> {
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
