//! HTTP client for the worker's `/admin/accounts*` routes.
//!
//! Auth is the `X-Admin-Key` header, distinct from the `ENCRYPTION_KEY`
//! used by `encrypt`/`status`/etc. — leaking the intent-auth key must
//! not let anyone mutate the account index.
//!
//! All four routes return plain-text / YAML bodies (no JSON parsing
//! required on the CLI side). The CLI just forwards the body to stdout
//! on success and surfaces the worker's error string on failure.

use std::time::Duration;

use color_eyre::eyre::{Result, eyre};
use trade_control_core::account::AccountMetadata;

const ADMIN_KEY_HEADER: &str = "X-Admin-Key";

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
