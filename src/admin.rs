//! Admin routes for the first-class account system.
//!
//! Gated by an `X-Admin-Key` header whose value must equal the
//! `ADMIN_KEY` secret. Distinct from `SIGNING_KEY` (intent auth)
//! and the diag key (which today reuses `SIGNING_KEY`) — credential
//! write paths get their own auth so a leaked diag/intent key can't
//! pivot into account mutation.
//!
//! Routes:
//!
//! - `GET    /admin/accounts`           — list account metadata as YAML
//! - `POST   /admin/accounts`           — add an account (JSON body)
//! - `DELETE /admin/accounts/<name>`    — remove an account from index
//! - `POST   /admin/accounts/<name>/test` — verify creds resolve and
//!   broker matches metadata
//!
//! `POST /admin/accounts` only writes the *metadata* — the credential
//! secret must be set separately via `wrangler secret put`. The two
//! operations are deliberately decoupled so the operator's wrangler
//! credential never touches the worker's request path.

#[cfg(target_arch = "wasm32")]
use trade_control_core::account::CredentialsError;
use trade_control_core::account::{AccountMetadata, MetadataError};
use worker::{Env, Request, Response, Result, console_error, console_log};

use crate::KV_NAMESPACE;
use crate::accounts::KvMetadataStore;
#[cfg(target_arch = "wasm32")]
use crate::accounts::SecretCredentialsResolver;

const ADMIN_KEY_HEADER: &str = "X-Admin-Key";
const ADMIN_KEY_SECRET: &str = "ADMIN_KEY";

/// True when the request authenticated against the `ADMIN_KEY` secret
/// via the `X-Admin-Key` header.
fn admin_key_ok(req: &Request, env: &Env) -> bool {
    let provided = match req.headers().get(ADMIN_KEY_HEADER) {
        Ok(Some(v)) => v,
        _ => return false,
    };
    let expected = match crate::get_secret(ADMIN_KEY_SECRET, env) {
        Some(s) => s,
        None => return false,
    };
    provided.len() == expected.len() && provided == expected
}

fn unauthorized() -> Result<Response> {
    Response::error("unauthorized", 401)
}

fn metadata_store(env: &Env) -> Result<KvMetadataStore> {
    let kv = env
        .kv(KV_NAMESPACE)
        .map_err(|e| worker::Error::RustError(format!("kv binding missing: {e:?}")))?;
    Ok(KvMetadataStore::new(kv))
}

/// Map a `MetadataError` to an HTTP response. NotFound → 404,
/// AlreadyExists → 409, anything else → 500 with the message.
fn metadata_error_to_response(err: MetadataError) -> Result<Response> {
    match err {
        MetadataError::NotFound(name) => Response::error(format!("not found: {name}"), 404),
        MetadataError::AlreadyExists(name) => {
            Response::error(format!("already exists: {name}"), 409)
        }
        MetadataError::Backend(msg) => {
            console_error!("admin metadata backend: {msg}");
            Response::error("state error", 500)
        }
    }
}

/// `GET /admin/accounts` — return the index as YAML.
pub async fn handle_list(req: &Request, env: &Env) -> Result<Response> {
    if !admin_key_ok(req, env) {
        return unauthorized();
    }
    let store = metadata_store(env)?;
    use trade_control_core::account::MetadataStore;
    let listed = match store.list().await {
        Ok(v) => v,
        Err(e) => return metadata_error_to_response(e),
    };
    match serde_yaml::to_string(&listed) {
        Ok(yaml) => Response::ok(yaml),
        Err(e) => {
            console_error!("admin list serialise: {e}");
            Response::error("internal error", 500)
        }
    }
}

/// `POST /admin/accounts` — add an account from the JSON body.
///
/// Body shape: a JSON-serialised `AccountMetadata`. The operator
/// composes this client-side (typically the future `account add` CLI
/// verb) — the worker doesn't prompt or fill defaults beyond what
/// `AccountMetadata`'s serde defaults provide (e.g. empty caps).
pub async fn handle_add(req: &mut Request, env: &Env) -> Result<Response> {
    if !admin_key_ok(req, env) {
        return unauthorized();
    }
    let body = req.text().await?;
    let metadata: AccountMetadata = match serde_json::from_str(&body) {
        Ok(m) => m,
        Err(e) => return Response::error(format!("bad metadata: {e}"), 400),
    };
    let store = metadata_store(env)?;
    use trade_control_core::account::MetadataStore;
    match store.add(metadata.clone()).await {
        Ok(()) => {
            console_log!(
                "admin: added account {} (broker={:?}, kind={:?})",
                metadata.name,
                metadata.broker,
                metadata.kind
            );
            Response::ok(format!("added: {}\n", metadata.name))
        }
        Err(e) => metadata_error_to_response(e),
    }
}

/// `DELETE /admin/accounts/<name>` — remove from index.
///
/// Note: this only removes the metadata entry. The credential secret
/// binding (`TN_ACCOUNT_<NAME>` / `OANDA_ACCOUNT_<NAME>`) lives in
/// Cloudflare Secret Store and must be cleared with
/// `wrangler secret delete` separately. Two operations is the price
/// of keeping the credential write path off the worker.
pub async fn handle_remove(req: &Request, env: &Env, name: &str) -> Result<Response> {
    if !admin_key_ok(req, env) {
        return unauthorized();
    }
    let store = metadata_store(env)?;
    use trade_control_core::account::MetadataStore;
    match store.remove(name).await {
        Ok(()) => {
            console_log!("admin: removed account {name} from index");
            Response::ok(format!(
                "removed: {name}\nremember to clear the credential secret with wrangler\n"
            ))
        }
        Err(e) => metadata_error_to_response(e),
    }
}

/// `POST /admin/accounts/<name>/test` — verify that the account's
/// metadata and credential secret are wired up and consistent.
///
/// Does **not** attempt a broker login — that's deferred to step 4
/// (live login implementation). For now this just confirms:
///
/// 1. Metadata exists for `name`.
/// 2. The corresponding credential secret binding exists and parses.
/// 3. The credential payload's broker matches the metadata's broker.
///
/// Returns YAML reporting what was verified, so the future `account
/// test` CLI verb can show a green/red status without parsing strings.
///
/// We don't wire this through `AccountStore::resolve` because the
/// resolver needs the metadata store to look up the broker, and
/// composing both behind `AccountStore` would require ownership
/// gymnastics for a single one-shot check. The flow here mirrors
/// `AccountStore::resolve` directly.
#[cfg(target_arch = "wasm32")]
pub async fn handle_test(req: &Request, env: &Env, name: &str) -> Result<Response> {
    use trade_control_core::account::{Credentials, MetadataStore};
    use trade_control_core::intent::BrokerKind;

    if !admin_key_ok(req, env) {
        return unauthorized();
    }
    let store = metadata_store(env)?;
    let meta = match store.get(name).await {
        Ok(m) => m,
        Err(e) => return metadata_error_to_response(e),
    };

    // OANDA accounts have no per-account credential secret — the
    // shared worker-wide `OANDA_API_KEY` covers every sub-account.
    // The metadata is the only thing to check.
    if meta.broker == BrokerKind::Oanda {
        if meta.oanda_account_id.is_none() {
            console_error!("admin test {name}: oanda metadata missing oanda_account_id");
            return Response::error(
                "oanda account missing `oanda_account_id` in metadata — re-run \
                 `trade-control account add`",
                424,
            );
        }
        console_log!(
            "admin: test {name} ok (broker=oanda, kind={}, account_id={})",
            wire_kind(meta.kind),
            meta.oanda_account_id.as_deref().unwrap_or("?")
        );
        let body = format!(
            "name: {}\nbroker: oanda\nkind: {}\noanda_account_id: {}\nstatus: ok\n",
            meta.name,
            wire_kind(meta.kind),
            meta.oanda_account_id.as_deref().unwrap_or("")
        );
        return Response::ok(body);
    }

    let resolver = SecretCredentialsResolver::new(env, &store);
    use trade_control_core::account::CredentialsResolver;
    let creds = match resolver.resolve(name).await {
        Ok(c) => c,
        Err(e) => return creds_error_to_response(name, e),
    };
    let actual = match creds {
        Credentials::TradeNation(_) => BrokerKind::TradeNation,
        Credentials::Oanda(_) => BrokerKind::Oanda,
    };
    if actual != meta.broker {
        console_error!(
            "admin test {name}: broker mismatch meta={:?} cred={:?}",
            meta.broker,
            actual
        );
        // Surface lowercase wire-form variant names so the operator
        // sees the same shape they sent in the JSON body, not Rust's
        // `Debug` rendering.
        let meta_str = wire_broker(meta.broker);
        let actual_str = wire_broker(actual);
        return Response::error(
            format!("broker mismatch: metadata says {meta_str} but credential is {actual_str}"),
            409,
        );
    }
    console_log!(
        "admin: test {name} ok (broker={}, kind={})",
        wire_broker(meta.broker),
        wire_kind(meta.kind)
    );
    // Don't echo the credential payload — only the metadata. Leaks
    // of `account test` output (logs, screenshots) should never
    // expose passwords. Use the lowercase wire form so the response
    // matches the YAML emitted by `/admin/accounts` for `list`.
    let body = format!(
        "name: {}\nbroker: {}\nkind: {}\nstatus: ok\n",
        meta.name,
        wire_broker(meta.broker),
        wire_kind(meta.kind)
    );
    Response::ok(body)
}

/// Lowercase wire-form variant of `BrokerKind`. Matches the
/// `#[serde(rename_all = "lowercase")]` shape used everywhere else, so
/// the admin response body is consistent with the YAML index.
#[cfg(target_arch = "wasm32")]
fn wire_broker(broker: trade_control_core::intent::BrokerKind) -> &'static str {
    use trade_control_core::intent::BrokerKind;
    match broker {
        BrokerKind::Oanda => "oanda",
        BrokerKind::TradeNation => "tradenation",
    }
}

/// Lowercase wire-form variant of `AccountKind`.
#[cfg(target_arch = "wasm32")]
fn wire_kind(kind: trade_control_core::account::AccountKind) -> &'static str {
    use trade_control_core::account::AccountKind;
    match kind {
        AccountKind::Demo => "demo",
        AccountKind::Live => "live",
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn handle_test(_req: &Request, _env: &Env, _name: &str) -> Result<Response> {
    // Native builds (tests) don't have a working secret-binding fetch.
    // The wasm path above is the production codepath; this stub keeps
    // the native build green.
    Response::error("admin test only available in wasm builds", 501)
}

#[cfg(target_arch = "wasm32")]
fn creds_error_to_response(name: &str, err: CredentialsError) -> Result<Response> {
    match err {
        CredentialsError::NotFound(_) => Response::error(
            format!("credential secret missing for {name}; set it via wrangler"),
            424, // Failed Dependency — index exists, secret doesn't
        ),
        CredentialsError::Malformed { reason, .. } => {
            Response::error(format!("credential secret malformed: {reason}"), 400)
        }
        CredentialsError::Backend(msg) => {
            console_error!("admin test {name}: creds backend: {msg}");
            Response::error("internal error", 500)
        }
    }
}

/// `POST /admin/adopt-trade` — register an externally-opened broker
/// position so the worker manages it from here on.
///
/// Body: JSON [`AdoptRequest`]. Verifies the position against the
/// live broker (instrument + direction + ids must all line up), then
/// writes a synthetic [`EntryAttempt`] keyed by `(account, trade_id,
/// 1)` so every other path (close, pause/resume, retry-gate,
/// SL-breach sweep) reads it identically to a self-placed row.
///
/// Native builds get a 501 stub — the wasm handler depends on
/// `acquire_tn_broker` / `tradenation_api` which only build under
/// `wasm32`. See `handle_test` for the same pattern.
/// Bridge `tradenation_api::Position` to the broker-agnostic
/// [`crate::adopt::BrokerPosition`] view. Kept here (not in
/// `adopt.rs`) so the pure-helper module doesn't depend on
/// `tradenation_api` and its unit tests stay native-runnable.
#[cfg(target_arch = "wasm32")]
impl crate::adopt::BrokerPosition for tradenation_api::Position {
    fn order_id(&self) -> String {
        self.order_id.to_string()
    }
    fn position_id(&self) -> String {
        self.position_id.to_string()
    }
    fn market_name(&self) -> &str {
        &self.market_name
    }
    fn direction_label(&self) -> &str {
        &self.direction
    }
}

#[cfg(target_arch = "wasm32")]
pub async fn handle_adopt(req: &mut Request, env: &Env) -> Result<Response> {
    use trade_control_core::account::MetadataStore;
    use trade_control_core::intent::BrokerKind;
    use trade_control_core::state::{EntryAttempt, StateStore};

    use crate::adopt::{AdoptRequest, VerifyOutcome, resolve_not_after, verify_position};

    if !admin_key_ok(req, env) {
        return unauthorized();
    }
    let body = req.text().await?;
    let adopt: AdoptRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return Response::error(format!("bad adopt request: {e}"), 400),
    };

    // Step 1: confirm the account exists and routes to a supported
    // broker. OANDA-side adoption isn't wired yet (the worker
    // separately accesses OANDA per-account already; adding the
    // verify path is mechanical but out of scope here). Reject
    // explicitly so the operator gets a clear error rather than a
    // silent write.
    let store = metadata_store(env)?;
    let meta = match store.get(&adopt.account).await {
        Ok(m) => m,
        Err(e) => return metadata_error_to_response(e),
    };
    if meta.broker != BrokerKind::TradeNation {
        return Response::error(
            format!(
                "adopt-trade only supports tradenation accounts in v1; \
                 {} routes to {:?}",
                adopt.account, meta.broker
            ),
            501,
        );
    }

    // Step 2: spin up a TN broker session (reuses the cron-warmed
    // cache when fresh) and pull the live positions.
    let broker = match crate::acquire_tn_broker(env, Some(&adopt.account)).await {
        Some(b) => b,
        None => {
            return Response::error(
                "tradenation login failed (missing account, bad credentials, or expired \
                 session — check worker logs)",
                503,
            );
        }
    };
    let details = match tradenation_api::get_account_details(broker.session()).await {
        Ok(d) => d,
        Err(err) => {
            console_error!(
                "adopt-trade[{}] get_account_details: {err:?}",
                adopt.account
            );
            return Response::error("broker lookup failed", 502);
        }
    };

    // Step 3: verify. Mismatch is a hard 409 — write nothing.
    let verdict = verify_position(&adopt, &details.positions.records);
    if verdict != VerifyOutcome::Ok {
        console_error!(
            "adopt-trade[{}] verify failed for trade_id={}: {}",
            adopt.account,
            adopt.trade_id,
            verdict.reason()
        );
        return Response::error(verdict.reason(), 409);
    }

    // Step 4: build the EntryAttempt and write it.
    let now = chrono::Utc::now();
    let state = crate::state::KvStateStore::new(
        env.kv(crate::KV_NAMESPACE)
            .map_err(|e| worker::Error::RustError(format!("kv binding missing: {e:?}")))?,
    );
    let snapshot = match state.snapshot().await {
        Ok(s) => s,
        Err(e) => {
            console_error!("adopt-trade[{}] snapshot: {e}", adopt.account);
            return Response::error("state read failed", 500);
        }
    };
    let not_after = resolve_not_after(&adopt.trade_id, &snapshot.recent_seen, now);
    let expires_at = not_after
        + chrono::Duration::seconds(trade_control_core::state::MIN_TTL_SECONDS as i64)
        + chrono::Duration::hours(1);

    let attempt = EntryAttempt {
        trade_id: adopt.trade_id.clone(),
        account: Some(adopt.account.clone()),
        instrument: adopt.instrument.clone(),
        attempt_no: 1,
        broker_order_id: adopt.broker_order_id.clone(),
        broker_trade_id: Some(adopt.broker_trade_id.clone()),
        direction: adopt.direction,
        placed_at: now,
        shell_time: now,
        expires_at,
        stop_loss_price: adopt.stop_loss_price,
    };

    if let Err(err) = state.record_entry_attempt(attempt.clone()).await {
        console_error!("adopt-trade[{}] record_entry_attempt: {err}", adopt.account);
        return Response::error("state write failed", 500);
    }

    console_log!(
        "admin: adopted trade_id={} on {} ({} {:?}) order_id={} position_id={} \
         expires_at={}",
        adopt.trade_id,
        adopt.account,
        adopt.instrument,
        adopt.direction,
        adopt.broker_order_id,
        adopt.broker_trade_id,
        expires_at.to_rfc3339()
    );

    // Echo the row back as YAML so the operator can sanity-check.
    let yaml = serde_yaml::to_string(&attempt)
        .map_err(|e| worker::Error::RustError(format!("encode entry_attempt yaml: {e}")))?;
    Response::ok(format!("status: adopted\n{yaml}"))
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn handle_adopt(_req: &mut Request, _env: &Env) -> Result<Response> {
    Response::error("admin adopt-trade only available in wasm builds", 501)
}
