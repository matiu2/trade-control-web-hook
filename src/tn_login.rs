//! Wasm-side TradeNation demo + live login.
//!
//! Wasm-only by construction: every call goes through `worker::Fetch`,
//! which has no native shim. The whole module is gated to
//! `cfg(target_arch = "wasm32")` to keep the native test binary from
//! linking `worker` into a non-existent JS runtime.
//!
//! The native login path in `tradenation_api` walks a redirect chain off
//! `practice.tradenation.com/finlogin/loginws.aspx` and scrapes
//! `Set-Cookie` headers per hop. `reqwest`'s wasm shim hides those
//! headers (it auto-follows redirects with no manual-redirect option),
//! so we drive the chain directly with `worker::Fetch` +
//! `RequestRedirect::Manual` instead, which surfaces every hop's
//! `Set-Cookie` via `Headers::get_all("set-cookie")`.
//!
//! Result: a [`tradenation_api::Session`] ready to be JSON-serialised
//! and handed to `broker_tradenation::login`.
//!
//! Two flows:
//!
//! - **Demo** ([`login_demo`]) — single redirect chain off
//!   `loginws.aspx?username=…&password=…`. Empirically the chain is:
//!     1. `loginws.aspx?username=…&password=…` → 302
//!     2. `innit.aspx?...&session_id=...`       → 302  (sets ASP.NET_SessionId)
//!     3. `Advanced.aspx?ots=...`               → 302  (sets ASP.NET_SessionId + OTS)
//!     4. `/`                                    → 200
//!
//! - **Live** ([`login_live`]) — JWT + auth0 + cloudtrade hops, then
//!   the redirect-chain harvest on the platform URL. Live writes
//!   require an OTS cookie, so the platform-bootstrap step rejects
//!   sessions that come back without one. Chain:
//!     1. POST `tradenation.com/signup/api/login` (JSON body) → JWT
//!     2. GET `portal.cube.finsatechnology.com/auth0/user` (Bearer)
//!        → `app_metadata.trading_accounts[]`; pick first funded
//!        active, else first active.
//!     3. POST `portal.cube.finsatechnology.com/cloudtrade/login`
//!        (Bearer + JSON `{account_id}`) → one-time platform URL.
//!     4. Follow the platform URL via the same `follow_redirect_chain`
//!        helper as demo, harvesting `ASP.NET_SessionId` + OTS.

use tradenation_api::Session;
use worker::{Fetch, Headers, Method, Request, RequestInit, RequestRedirect, Result, Url};

const MAX_HOPS: usize = 10;
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:148.0) Gecko/20100101 Firefox/148.0";

/// Log in to the TradeNation demo platform with credentials, returning
/// a freshly-minted `Session`.
///
/// Returns `Err` if the redirect chain doesn't yield both an
/// `ASP.NET_SessionId` and an OTS cookie — that indicates the
/// credentials are bad, TN changed the flow, or the worker's outbound
/// fetch hit a transient failure.
pub async fn login_demo(username: &str, password: &str) -> Result<Session> {
    let mut url = Url::parse("https://practice.tradenation.com/finlogin/loginws.aspx")
        .map_err(|e| worker::Error::RustError(format!("parse loginws url: {e}")))?;
    url.query_pairs_mut()
        .append_pair("username", username)
        .append_pair("password", password);

    let cookies = follow_redirect_chain(url.as_str()).await?;
    let (asp_session_id, ots) = extract_session_cookies(&cookies)
        .map_err(|e| worker::Error::RustError(format!("tn login: {e}")))?;

    Ok(Session::demo(
        username,
        password,
        &asp_session_id,
        ots.as_ref(),
    ))
}

const LIVE_LOGIN_URL: &str = "https://tradenation.com/signup/api/login";
const LIVE_AUTH0_USER_URL: &str = "https://portal.cube.finsatechnology.com/auth0/user";
const LIVE_CLOUDTRADE_LOGIN_URL: &str = "https://portal.cube.finsatechnology.com/cloudtrade/login";

/// Log in to the TradeNation **live** platform.
///
/// Drives the JWT → auth0 → cloudtrade → platform-redirect chain
/// described in the module docs. Returns a `Session::live` on success.
///
/// Errors when any hop fails, when no active trading account is
/// returned, or when the platform-bootstrap step doesn't produce both
/// an `ASP.NET_SessionId` and an OTS cookie. OTS is mandatory for live
/// — order writes use it as the request `key`, so a session without
/// one would silently fail at trade time. Better to refuse here.
pub async fn login_live(username: &str, password: &str) -> Result<Session> {
    let access_token = get_jwt(username, password).await?;
    let account_id = pick_account_id_from_jwt(&access_token).await?;
    rlog!("tn live login: selected account_id={account_id}");
    let platform_url = get_platform_url(&access_token, account_id).await?;
    let (asp_session_id, ots_name, ots_value) = bootstrap_live_session(&platform_url).await?;
    Ok(Session::live(
        &access_token,
        &asp_session_id,
        &ots_name,
        &ots_value,
    ))
}

/// POST credentials, receive `{ "access_token": "..." }`.
async fn get_jwt(username: &str, password: &str) -> Result<String> {
    let body = serde_json::json!({ "username": username, "password": password }).to_string();
    let headers = Headers::new();
    headers.set("User-Agent", USER_AGENT)?;
    headers.set("Content-Type", "application/json")?;
    headers.set("Origin", "https://tradenation.com")?;
    headers.set("Referer", "https://tradenation.com/login")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_redirect(RequestRedirect::Follow)
        .with_headers(headers)
        .with_body(Some(body.into()));

    let req = Request::new_with_init(LIVE_LOGIN_URL, &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    let status = resp.status_code();
    let text = resp.text().await?;
    if !(200..300).contains(&status) {
        return Err(worker::Error::RustError(format!(
            "tn live login: signup/api/login {status}: {}",
            truncate_for_log(&text)
        )));
    }
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| worker::Error::RustError(format!("tn live login: parse JWT json: {e}")))?;
    parsed
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or_else(|| {
            worker::Error::RustError("tn live login: response missing access_token".into())
        })
}

/// Call `auth0/user` and select a trading account id. Preference:
/// first active account with `balance.cash_balance` > 0, else first
/// active account. Mirrors the native `get_account_id` in
/// `tradenation_api`.
async fn pick_account_id_from_jwt(access_token: &str) -> Result<u64> {
    let headers = Headers::new();
    headers.set("User-Agent", USER_AGENT)?;
    headers.set("Authorization", &format!("Bearer {access_token}"))?;
    headers.set("Origin", "https://tradenation.com")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Get)
        .with_redirect(RequestRedirect::Follow)
        .with_headers(headers);

    let req = Request::new_with_init(LIVE_AUTH0_USER_URL, &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    let status = resp.status_code();
    let text = resp.text().await?;
    if !(200..300).contains(&status) {
        return Err(worker::Error::RustError(format!(
            "tn live login: auth0/user {status}: {}",
            truncate_for_log(&text)
        )));
    }
    pick_funded_account(&text)
}

/// Thin wasm-side wrapper over [`crate::tn_login_helpers::pick_funded_account`].
/// The helper is pure (no `worker` types) and host-testable; we just
/// remap its `color_eyre` error into `worker::Error::RustError` here.
fn pick_funded_account(json: &str) -> Result<u64> {
    crate::tn_login_helpers::pick_funded_account(json).map_err(worker::Error::RustError)
}

/// POST to `cloudtrade/login` with the chosen account id and read back
/// the one-time platform login URL.
async fn get_platform_url(access_token: &str, account_id: u64) -> Result<String> {
    let body = serde_json::json!({ "account_id": account_id }).to_string();
    let headers = Headers::new();
    headers.set("User-Agent", USER_AGENT)?;
    headers.set("Authorization", &format!("Bearer {access_token}"))?;
    headers.set("Content-Type", "application/json")?;
    headers.set("Origin", "https://tradenation.com")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_redirect(RequestRedirect::Follow)
        .with_headers(headers)
        .with_body(Some(body.into()));

    let req = Request::new_with_init(LIVE_CLOUDTRADE_LOGIN_URL, &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    let status = resp.status_code();
    let text = resp.text().await?;
    if !(200..300).contains(&status) {
        return Err(worker::Error::RustError(format!(
            "tn live login: cloudtrade/login {status}: {}",
            truncate_for_log(&text)
        )));
    }
    let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        worker::Error::RustError(format!("tn live login: parse cloudtrade/login json: {e}"))
    })?;
    let url = parsed.get("url").and_then(|v| v.as_str()).ok_or_else(|| {
        worker::Error::RustError("tn live login: cloudtrade/login response missing url".into())
    })?;
    if url.is_empty() {
        return Err(worker::Error::RustError(
            "tn live login: cloudtrade/login returned empty url".into(),
        ));
    }
    Ok(url.to_owned())
}

/// Follow the platform login URL and extract `(ASP.NET_SessionId,
/// ots_name, ots_value)`. Live writes use the OTS value as the
/// request `key`, so a missing OTS is fatal here (unlike demo, where
/// some flows complete without one).
async fn bootstrap_live_session(platform_url: &str) -> Result<(String, String, String)> {
    let cookies = follow_redirect_chain(platform_url).await?;
    let (asp_session_id, ots) = extract_session_cookies(&cookies)
        .map_err(|e| worker::Error::RustError(format!("tn live login: {e}")))?;
    let (ots_name, ots_value) = ots.ok_or_else(|| {
        worker::Error::RustError(
            "tn live login: OTS cookie missing — live writes require it".into(),
        )
    })?;
    Ok((asp_session_id, ots_name, ots_value))
}

// truncate_for_log lives in `tn_login_helpers` so it can be host-tested
// alongside `pick_funded_account`.
use crate::tn_login_helpers::truncate_for_log;

/// Walk the redirect chain manually, collecting every cookie set on the way.
async fn follow_redirect_chain(start_url: &str) -> Result<Vec<(String, String)>> {
    let mut url = start_url.to_owned();
    let mut cookies = Vec::new();

    for hop in 0..MAX_HOPS {
        let headers = Headers::new();
        headers.set("User-Agent", USER_AGENT)?;

        let mut init = RequestInit::new();
        init.with_method(Method::Get)
            .with_redirect(RequestRedirect::Manual)
            .with_headers(headers);

        let req = Request::new_with_init(&url, &init)?;
        let resp = Fetch::Request(req).send().await?;
        let status = resp.status_code();

        for cookie_line in resp.headers().get_all("set-cookie")? {
            if let Some((name, value)) = parse_set_cookie(&cookie_line) {
                rlog!("tn login hop {hop}: cookie {name}");
                cookies.push((name, value));
            }
        }

        // RequestRedirect::Manual surfaces 30x verbatim with the Location
        // header readable. Anything outside 300..400 ends the chain.
        if !(300..400).contains(&status) {
            return Ok(cookies);
        }

        let Some(location) = resp.headers().get("location")? else {
            return Err(worker::Error::RustError(
                "tn login: redirect missing Location header".into(),
            ));
        };

        url = resolve_location(&url, &location)?;
    }

    Err(worker::Error::RustError(format!(
        "tn login: exceeded {MAX_HOPS} redirects"
    )))
}

/// Resolve a `Location` value (absolute or relative) against the current url.
fn resolve_location(current: &str, location: &str) -> Result<String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Ok(location.to_owned());
    }
    let base = Url::parse(current)
        .map_err(|e| worker::Error::RustError(format!("tn login: parse current url: {e}")))?;
    let next = base
        .join(location)
        .map_err(|e| worker::Error::RustError(format!("tn login: join location: {e}")))?;
    Ok(next.to_string())
}

/// Parse a single `Set-Cookie` header line and return `(name, value)`.
/// Cookie attributes (Path, Domain, HttpOnly, …) are discarded — we only
/// need the name/value pair for session reconstruction.
fn parse_set_cookie(line: &str) -> Option<(String, String)> {
    let first = line.split(';').next()?.trim();
    let (name, value) = first.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some((name.to_owned(), value.trim().to_owned()))
}

/// Pull the ASP.NET session id and the 8-char-uppercase OTS cookie out of
/// the redirect-chain cookie list.
///
/// - `ASP.NET_SessionId`: take the **last** one set in the chain (later
///   hops re-bind it; the value we need is the one in effect at end).
/// - OTS: any cookie whose name is exactly 8 ASCII-uppercase letters.
///   Some demo flows complete without ever setting one (the order-write
///   endpoints fail without it though), so we return `Option`.
fn extract_session_cookies(
    cookies: &[(String, String)],
) -> std::result::Result<(String, Option<(String, String)>), String> {
    let asp_session_id = cookies
        .iter()
        .rev()
        .find(|(name, _)| name == "ASP.NET_SessionId")
        .map(|(_, v)| v.clone())
        .ok_or_else(|| "ASP.NET_SessionId not found in redirect chain".to_owned())?;

    let ots = cookies
        .iter()
        .find(|(name, _)| name.len() == 8 && name.chars().all(|c| c.is_ascii_uppercase()))
        .map(|(n, v)| (n.clone(), v.clone()));

    if ots.is_none() {
        rlog_err!(
            "tn login: ASP.NET_SessionId captured but OTS cookie missing — order writes will fail"
        );
    }

    Ok((asp_session_id, ots))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_cookie() {
        assert_eq!(
            parse_set_cookie("ASP.NET_SessionId=abc123; path=/; HttpOnly"),
            Some(("ASP.NET_SessionId".into(), "abc123".into()))
        );
    }

    #[test]
    fn parses_ots_cookie_with_special_chars() {
        // Real OTS values can contain '+' / '/' / '='.
        assert_eq!(
            parse_set_cookie("DLDEGFUL=vLkEf+OQ47i+abc/def==; path=/"),
            Some(("DLDEGFUL".into(), "vLkEf+OQ47i+abc/def==".into()))
        );
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(parse_set_cookie(""), None);
        assert_eq!(parse_set_cookie("no-equals; path=/"), None);
        assert_eq!(parse_set_cookie("=novalue; path=/"), None);
    }

    #[test]
    fn extracts_last_asp_session_id() {
        let cookies = vec![
            ("ASP.NET_SessionId".into(), "first".into()),
            ("DLDEGFUL".into(), "ots-val".into()),
            ("ASP.NET_SessionId".into(), "second".into()),
        ];
        let (asp, ots) = extract_session_cookies(&cookies).unwrap();
        assert_eq!(asp, "second");
        assert_eq!(ots, Some(("DLDEGFUL".into(), "ots-val".into())));
    }

    #[test]
    fn extract_errors_without_asp_session_id() {
        let cookies = vec![("DLDEGFUL".into(), "ots-val".into())];
        assert!(extract_session_cookies(&cookies).is_err());
    }

    #[test]
    fn extract_handles_missing_ots() {
        let cookies = vec![("ASP.NET_SessionId".into(), "abc".into())];
        let (asp, ots) = extract_session_cookies(&cookies).unwrap();
        assert_eq!(asp, "abc");
        assert!(ots.is_none());
    }

    #[test]
    fn ignores_non_uppercase_8char_names() {
        let cookies = vec![
            ("ASP.NET_SessionId".into(), "abc".into()),
            ("lowercase".into(), "x".into()), // 9 chars, not uppercase
            ("AWSALB".into(), "x".into()),    // 6 chars
            ("VeryLong".into(), "x".into()),  // mixed case
            ("ABCDEFGH".into(), "ots".into()), // 8 uppercase — match
        ];
        let (_, ots) = extract_session_cookies(&cookies).unwrap();
        assert_eq!(ots, Some(("ABCDEFGH".into(), "ots".into())));
    }

    #[test]
    fn resolves_absolute_location() {
        let out = resolve_location("https://a.example/path", "https://b.example/foo").unwrap();
        assert_eq!(out, "https://b.example/foo");
    }

    #[test]
    fn resolves_relative_location() {
        let out = resolve_location("https://a.example/dir/page", "../other?x=1").unwrap();
        assert_eq!(out, "https://a.example/other?x=1");
    }

    // pick_funded_account / truncate_for_log are tested in
    // `tn_login_helpers` (host target), since this module is
    // wasm-only and doesn't run under `cargo test`.
}
