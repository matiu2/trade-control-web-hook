//! Read-only diagnostic endpoints. Gated by an `X-Diag-Key` header
//! whose value must equal the `SIGNING_KEY` secret (re-used so
//! there's only one intent-auth secret to manage).
//!
//! Routes:
//!
//! - `GET /diag/fx?from=GBP&to=USD` — runs `tradenation_api::fx_rate`
//!   against the currently-cached TN session, returns YAML with the
//!   resolved rate. Lets the operator reproduce a sizing failure
//!   without firing a real entry.
//! - `GET /diag/candles?market_id=71410&type=bid&tf=minute&count=1` —
//!   hits the unauthenticated `charts.finsatechnology.com` endpoint
//!   directly via the broker's `reqwest::Client`. Lets us verify the
//!   chart-data endpoint works from inside wasm before rewriting
//!   `fx_rate` to depend on it. `type` ∈ {bid, ask, mid}; `tf` ∈
//!   {minute, quarter, hour, day}; defaults: type=bid, tf=minute,
//!   count=1.

use worker::{Env, Request, Response, Result};

use crate::{SIGNING_KEY_SECRET, acquire_tn_broker, get_secret};

const DIAG_KEY_HEADER: &str = "X-Diag-Key";

/// True when the request authenticated against the `SIGNING_KEY`
/// secret via the `X-Diag-Key` header.
fn diag_key_ok(req: &Request, env: &Env) -> bool {
    let provided = match req.headers().get(DIAG_KEY_HEADER) {
        Ok(Some(v)) => v,
        _ => return false,
    };
    let expected = match get_secret(SIGNING_KEY_SECRET, env) {
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
    let mut account: Option<String> = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "from" => from = Some(v.into_owned()),
            "to" => to = Some(v.into_owned()),
            "account" => account = Some(v.into_owned()),
            _ => {}
        }
    }
    let Some(from) = from else {
        return Response::error("missing query parameter: from", 400);
    };
    let Some(to) = to else {
        return Response::error("missing query parameter: to", 400);
    };

    let broker = match acquire_tn_broker(env, account.as_deref()).await {
        Some(b) => b,
        None => return Response::error("tradenation session unavailable", 503),
    };

    rlog!("diag fx_rate {from}->{to}");
    match tradenation_api::fx_rate(broker.client(), broker.session(), &from, &to).await {
        Ok(rate) => {
            let body = format!("from: {from}\nto: {to}\nrate: {rate}\n");
            Response::ok(body)
        }
        Err(err) => {
            let body = format!("from: {from}\nto: {to}\nerror: {err}\n");
            rlog_err!("diag fx_rate {from}->{to}: {err}");
            // 200 with body — this is a diagnostic, the worker is fine,
            // the upstream call failed and the operator wants the
            // detail.
            Response::ok(body)
        }
    }
}

const CHARTS_BASE: &str = "https://charts.finsatechnology.com/data";
const CHARTS_ORIGIN: &str = "https://chart-cfd.tradenation.com";

/// `GET /diag/candles?market_id=N&type=bid|ask|mid&tf=minute|quarter|hour|day&count=N`
/// — fetch raw OHLCV from charts.finsatechnology.com using the broker's
/// reqwest client. No auth needed at the endpoint; this verifies the
/// call shape works from inside wasm.
pub async fn handle_candles(req: &Request, env: &Env) -> Result<Response> {
    if !diag_key_ok(req, env) {
        return Response::error("unauthorized", 401);
    }

    let url = req.url()?;
    let mut market_id: Option<String> = None;
    let mut price_type: String = "bid".to_string();
    let mut tf: String = "minute".to_string();
    let mut count: String = "1".to_string();
    let mut account: Option<String> = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "market_id" => market_id = Some(v.into_owned()),
            "type" => price_type = v.into_owned(),
            "tf" => tf = v.into_owned(),
            "count" => count = v.into_owned(),
            "account" => account = Some(v.into_owned()),
            _ => {}
        }
    }
    let Some(market_id) = market_id else {
        return Response::error("missing query parameter: market_id", 400);
    };
    if !matches!(price_type.as_str(), "bid" | "ask" | "mid") {
        return Response::error("type must be bid, ask, or mid", 400);
    }
    if !matches!(tf.as_str(), "minute" | "quarter" | "hour" | "day") {
        return Response::error("tf must be minute, quarter, hour, or day", 400);
    }

    let broker = match acquire_tn_broker(env, account.as_deref()).await {
        Some(b) => b,
        None => return Response::error("tradenation session unavailable", 503),
    };

    let chart_url = format!("{CHARTS_BASE}/{tf}/{market_id}/{price_type}?l={count}");
    rlog!("diag candles GET {chart_url}");

    let resp = broker
        .client()
        .get(&chart_url)
        .header("Origin", CHARTS_ORIGIN)
        .header("Referer", format!("{CHARTS_ORIGIN}/"))
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status = r.status();
            match r.text().await {
                Ok(body) => Response::ok(format!(
                    "status: {status}\nurl: {chart_url}\nbody: {body}\n"
                )),
                Err(err) => Response::ok(format!(
                    "status: {status}\nurl: {chart_url}\nbody_error: {err}\n"
                )),
            }
        }
        Err(err) => {
            rlog_err!("diag candles {chart_url}: {err}");
            Response::ok(format!("url: {chart_url}\nerror: {err}\n"))
        }
    }
}
