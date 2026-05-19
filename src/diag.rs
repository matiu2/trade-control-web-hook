//! Read-only diagnostic endpoints. Gated by an `X-Diag-Key` header
//! whose value must equal the `ENCRYPTION_KEY` secret (re-used so
//! there's only one secret to manage).
//!
//! Routes:
//!
//! - `GET /diag/fx?from=GBP&to=USD` — runs `tradenation_api::fx_rate`
//!   against the currently-cached TN session, returns YAML with the
//!   resolved rate. Lets the operator reproduce a sizing failure
//!   without firing a real entry.

use worker::{Env, Request, Response, Result, console_error, console_log};

use crate::{ENCRYPTION_KEY_SECRET, acquire_tn_broker, get_secret};

const DIAG_KEY_HEADER: &str = "X-Diag-Key";

/// True when the request authenticated against the `ENCRYPTION_KEY`
/// secret via the `X-Diag-Key` header.
fn diag_key_ok(req: &Request, env: &Env) -> bool {
    let provided = match req.headers().get(DIAG_KEY_HEADER) {
        Ok(Some(v)) => v,
        _ => return false,
    };
    let expected = match get_secret(ENCRYPTION_KEY_SECRET, env) {
        Some(s) => s,
        None => return false,
    };
    // Constant-time-ish: compare full lengths first.
    provided.len() == expected.len() && provided == expected
}

/// `GET /diag/fx?from=GBP&to=USD` — resolve a live FX rate via the
/// cached TN session.
pub async fn handle_fx(req: &Request, env: &Env) -> Result<Response> {
    if !diag_key_ok(req, env) {
        return Response::error("unauthorized", 401);
    }

    let url = req.url()?;
    let mut from: Option<String> = None;
    let mut to: Option<String> = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "from" => from = Some(v.into_owned()),
            "to" => to = Some(v.into_owned()),
            _ => {}
        }
    }
    let Some(from) = from else {
        return Response::error("missing query parameter: from", 400);
    };
    let Some(to) = to else {
        return Response::error("missing query parameter: to", 400);
    };

    let broker = match acquire_tn_broker(env).await {
        Some(b) => b,
        None => return Response::error("tradenation session unavailable", 503),
    };

    console_log!("diag fx_rate {from}->{to}");
    match tradenation_api::fx_rate(broker.client(), broker.session(), &from, &to).await {
        Ok(rate) => {
            let body = format!("from: {from}\nto: {to}\nrate: {rate}\n");
            Response::ok(body)
        }
        Err(err) => {
            let body = format!("from: {from}\nto: {to}\nerror: {err}\n");
            console_error!("diag fx_rate {from}->{to}: {err}");
            // 200 with body — this is a diagnostic, the worker is fine,
            // the upstream call failed and the operator wants the
            // detail.
            Response::ok(body)
        }
    }
}
