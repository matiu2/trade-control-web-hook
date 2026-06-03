//! HTTP client for the worker's `/admin/accounts*` routes.
//!
//! Auth is the `X-Admin-Key` header, distinct from the `SIGNING_KEY`
//! used by `sign`/`status`/etc. — leaking the intent-auth key must
//! not let anyone mutate the account index.
//!
//! All four routes return plain-text / YAML bodies (no JSON parsing
//! required on the CLI side). The CLI just forwards the body to stdout
//! on success and surfaces the worker's error string on failure.

use std::time::Duration;

use color_eyre::eyre::{Result, eyre};
use serde::Serialize;
use trade_control_core::account::AccountMetadata;
use trade_control_core::intent::Direction;

const ADMIN_KEY_HEADER: &str = "X-Admin-Key";

/// Body shape for `POST /admin/adopt-trade`. Mirrors the worker-side
/// `AdoptRequest` (kept in `src/adopt.rs` on the worker). Duplicated
/// here rather than shared via a new crate — the wire format is small
/// and a copy in each direction is easier to read than a new dep.
#[derive(Debug, Clone, Serialize)]
pub struct AdoptBody {
    pub account: String,
    pub trade_id: String,
    pub instrument: String,
    pub direction: Direction,
    pub broker_order_id: String,
    pub broker_trade_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_loss_price: Option<f64>,
}

/// Build a `reqwest::blocking::Client` with a 30s timeout — same as
/// the encrypted-control client. Sharing the constant keeps timeout
/// behaviour uniform across the binary.
fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| eyre!("http client: {e}"))
}

/// Join `endpoint` with `path` without double-slashing. `endpoint` is the
/// worker base URL (e.g. `https://x.workers.dev`); `path` always starts
/// with `/`.
fn url(endpoint: &str, path: &str) -> String {
    let base = endpoint.trim_end_matches('/');
    format!("{base}{path}")
}

/// `GET /admin/accounts` — returns the YAML listing.
pub fn list_accounts(endpoint: &str, admin_key: &str) -> Result<String> {
    let client = http_client()?;
    let resp = client
        .get(url(endpoint, "/admin/accounts"))
        .header(ADMIN_KEY_HEADER, admin_key)
        .send()
        .map_err(|e| eyre!("GET /admin/accounts: {e}"))?;
    consume(resp)
}

/// `POST /admin/accounts` — adds a metadata record. Body is the JSON
/// serialisation of `AccountMetadata` (matches what the worker expects).
pub fn add_account(endpoint: &str, admin_key: &str, metadata: &AccountMetadata) -> Result<String> {
    let body = serde_json::to_string(metadata).map_err(|e| eyre!("encode metadata: {e}"))?;
    let client = http_client()?;
    let resp = client
        .post(url(endpoint, "/admin/accounts"))
        .header(ADMIN_KEY_HEADER, admin_key)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .map_err(|e| eyre!("POST /admin/accounts: {e}"))?;
    consume(resp)
}

/// `DELETE /admin/accounts/<name>` — removes a metadata record. The
/// credential secret must be removed separately via
/// `wrangler secret delete`; the worker can't reach into Secret Store
/// for that.
pub fn delete_account(endpoint: &str, admin_key: &str, name: &str) -> Result<String> {
    let client = http_client()?;
    let resp = client
        .delete(url(endpoint, &format!("/admin/accounts/{name}")))
        .header(ADMIN_KEY_HEADER, admin_key)
        .send()
        .map_err(|e| eyre!("DELETE /admin/accounts/{name}: {e}"))?;
    consume(resp)
}

/// `POST /admin/adopt-trade` — register an externally-opened broker
/// position so the worker manages it from here on. Returns the
/// echoed YAML on success.
pub fn adopt_trade(endpoint: &str, admin_key: &str, body: &AdoptBody) -> Result<String> {
    let json = serde_json::to_string(body).map_err(|e| eyre!("encode adopt body: {e}"))?;
    let client = http_client()?;
    let resp = client
        .post(url(endpoint, "/admin/adopt-trade"))
        .header(ADMIN_KEY_HEADER, admin_key)
        .header("content-type", "application/json")
        .body(json)
        .send()
        .map_err(|e| eyre!("POST /admin/adopt-trade: {e}"))?;
    consume(resp)
}

/// `POST /admin/accounts/<name>/test` — verify that metadata + credential
/// secret + broker tag line up. Returns the YAML status report.
pub fn test_account(endpoint: &str, admin_key: &str, name: &str) -> Result<String> {
    let client = http_client()?;
    let resp = client
        .post(url(endpoint, &format!("/admin/accounts/{name}/test")))
        .header(ADMIN_KEY_HEADER, admin_key)
        .send()
        .map_err(|e| eyre!("POST /admin/accounts/{name}/test: {e}"))?;
    consume(resp)
}

/// Surface the response body on success, or wrap status + body into an
/// error on failure. Body is included verbatim because the admin routes
/// already format errors as plain strings.
fn consume(resp: reqwest::blocking::Response) -> Result<String> {
    let status = resp.status();
    let text = resp.text().map_err(|e| eyre!("read response body: {e}"))?;
    if !status.is_success() {
        return Err(eyre!("worker returned {status}: {text}"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_join_handles_trailing_slash() {
        assert_eq!(
            url("https://x.workers.dev/", "/admin/accounts"),
            "https://x.workers.dev/admin/accounts"
        );
    }

    #[test]
    fn url_join_handles_no_trailing_slash() {
        assert_eq!(
            url("https://x.workers.dev", "/admin/accounts"),
            "https://x.workers.dev/admin/accounts"
        );
    }
}
