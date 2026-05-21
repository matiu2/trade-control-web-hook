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
