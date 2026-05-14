mod oanda;
mod risk;
mod state;

#[cfg(feature = "cli")]
pub mod cli;

use chrono::Utc;
use worker::{Context, Env, Request, Response, Result, console_error, console_log, event};

use crate::oanda::{
    EntryRequest, OANDA_ACCOUNT_ID, cancel_pending_for_instrument, close_positions, login,
    place_entry,
};
use crate::state::KvStateStore;
use serde::Serialize;
use trade_control_core::crypto;
use trade_control_core::incoming::{self, parse_and_verify};
use trade_control_core::intent::{Action, Resolved};
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

    // Status and Unlock are control actions that don't touch OANDA; handle
    // them up front so we don't waste an OANDA login on them.
    match verified.intent.action {
        Action::Status => return handle_status(&store, &verified, now).await,
        Action::Unlock => return handle_unlock(&store, &verified, now).await,
        _ => {}
    }

    let account_id = match get_secret(OANDA_ACCOUNT_ID, &env) {
        Some(s) => s,
        None => {
            console_error!("missing required secret: {OANDA_ACCOUNT_ID}");
            return Response::error("server misconfigured", 500);
        }
    };
    let Some(client) = login(&env).await else {
        return Response::error("oanda login failed", 500);
    };

    let result = match verified.intent.action {
        Action::Enter => {
            // Cooldown gate
            match store.is_cooled_down(&verified.intent.instrument).await {
                Ok(true) => {
                    console_log!(
                        "entry rejected: {} cooled down (id={})",
                        verified.intent.instrument,
                        verified.intent.id
                    );
                    return Response::error("instrument cooled down", 423);
                }
                Ok(false) => {}
                Err(err) => {
                    console_error!("KV is_cooled_down: {err}");
                    return Response::error("state error", 500);
                }
            }

            let max_risk_pct = secret_or_default(&env, MAX_RISK_PCT_PER_TRADE_SECRET, 1.0);
            let max_open_positions = secret_or_default(&env, MAX_OPEN_POSITIONS_SECRET, 3.0) as u32;
            let pip_size = pip_size_for(&env, &verified.intent.instrument);

            let resolved = match Resolved::from_intent(&verified.intent, &verified.shell, pip_size)
            {
                Ok(r) => r,
                Err(err) => {
                    console_error!("resolve: {err}");
                    return Response::error("rejected", 400);
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
            place_entry(
                &client,
                &account_id,
                max_risk_pct,
                max_open_positions,
                &entry_request,
            )
            .await
            .map(|order_id| {
                console_log!("entry placed id={} order={}", verified.intent.id, order_id);
            })
            .map_err(|err| {
                console_error!("entry failed: {err}");
                err.to_string()
            })
        }
        Action::Close => {
            let ok = close_positions(&client, &account_id, &verified.intent.instrument).await;
            if ok {
                Ok(())
            } else {
                Err("close failed".to_string())
            }
        }
        Action::Invalidate => {
            let hours = verified.intent.cooldown_hours.unwrap_or(12);
            if let Err(err) = store.set_cooldown(&verified.intent.instrument, hours).await {
                console_error!("KV set_cooldown: {err}");
                return Response::error("state error", 500);
            }
            let cancelled =
                cancel_pending_for_instrument(&client, &account_id, &verified.intent.instrument)
                    .await;
            console_log!(
                "invalidate {} cooldown {}h cancelled {} pending",
                verified.intent.instrument,
                hours,
                cancelled
            );
            Ok(())
        }
        Action::Status | Action::Unlock => {
            // Handled in an early-return above. Unreachable here.
            unreachable!("status/unlock handled before OANDA dispatch")
        }
    };

    match result {
        Ok(()) => {
            // Record id to block replays. Best-effort: if KV write fails after a successful
            // trade we log but still return 200 — failing here would invite the caller to retry
            // and re-execute the trade.
            let ttl = incoming::replay_ttl_seconds(verified.intent.not_after, now);
            if let Err(err) = store.mark_seen(&verified.intent.id, ttl).await {
                console_error!("KV mark_seen after success: {err}");
            }
            Response::ok("ok")
        }
        Err(_) => Response::error("action failed", 502),
    }
}

/// Handle the `status` action: dump cooldown + recent-seen indexes as YAML.
async fn handle_status(
    store: &KvStateStore,
    verified: &crate::incoming::Verified,
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
    let ttl = incoming::replay_ttl_seconds(verified.intent.not_after, now);
    if let Err(err) = store.mark_seen(&verified.intent.id, ttl).await {
        console_error!("KV mark_seen after status: {err}");
    }
    Response::ok(body)
}

/// Handle the `unlock` action: clear the cooldown for `verified.intent.instrument`.
async fn handle_unlock(
    store: &KvStateStore,
    verified: &crate::incoming::Verified,
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
    let ttl = incoming::replay_ttl_seconds(verified.intent.not_after, now);
    if let Err(err) = store.mark_seen(&verified.intent.id, ttl).await {
        console_error!("KV mark_seen after unlock: {err}");
    }
    Response::ok(body)
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
