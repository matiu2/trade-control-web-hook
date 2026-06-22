//! POST a signed `register` intent (carrying the whole [`TradePlan`]) to the
//! worker.
//!
//! This is tv-arm's direct HTTP path to the worker, and the only way a trade is
//! armed: tv-arm folds the whole trade into one [`TradePlan`] and registers it
//! up front, so the worker's cron engine can evaluate the conditions itself on
//! its tick. (The retired legacy path reached the worker only indirectly —
//! TradingView delivered the signed alert `message` when an alert fired.)
//!
//! The destination is the baked-at-build-time webhook (`BAKED_WEBHOOK`), so a
//! `tv-arm-staging` binary registers against the staging worker with no env var
//! or flag.
//!
//! A failed register is logged and returned as an error to `run`, but the signed
//! bundle is already on disk by then, so the trade isn't lost.

use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr, eyre};

/// The worker endpoint, baked in at build time by `build.rs` from
/// `TRADE_CONTROL_WEBHOOK`. Each per-environment binary embeds its own worker
/// URL (see the per-env deploy scripts).
const BAKED_WEBHOOK: &str = env!("BAKED_WEBHOOK");

/// Timeout for the register POST. Generous — the worker only validates +
/// records the plan, but a cold worker can take a couple of seconds.
const POST_TIMEOUT: Duration = Duration::from_secs(20);

/// POST the already-signed register body to the worker on a throwaway runtime.
///
/// `pipeline::run` is sync (it drives the whole arm flow), but `reqwest` is
/// async, so we spin a short-lived tokio runtime here — the same bridge
/// `resolve_mw_trade`'s live spread read uses. Any non-2xx status or transport
/// failure surfaces as an `Err` carrying the worker's response body so the
/// operator can see *why* the engine rejected the plan (bad trade_id match,
/// signature, etc.).
pub fn post_register_blocking(signed_body: String) -> Result<()> {
    post_intent_blocking(signed_body).map(|_| ())
}

/// POST any already-signed intent body to the worker and return the worker's
/// 2xx response body. Same destination + runtime bridge as
/// [`post_register_blocking`], but surfaces the response text — used by the
/// `--update` flow to read the `plan-list` YAML (a register POST discards it).
/// A non-2xx status or transport failure is an `Err` carrying the worker's
/// message.
pub fn post_intent_blocking(signed_body: String) -> Result<String> {
    let runtime =
        tokio::runtime::Runtime::new().wrap_err("starting tokio runtime for worker POST")?;
    runtime.block_on(post_intent(signed_body))
}

/// Async POST of the signed body, returning the worker's response text on 2xx.
/// Factored out so it can be exercised against a local mock in tests without
/// the runtime bridge.
async fn post_intent(signed_body: String) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(POST_TIMEOUT)
        .build()
        .wrap_err("build reqwest client")?;
    let resp = client
        .post(BAKED_WEBHOOK)
        .header("content-type", "text/plain")
        .body(signed_body)
        .send()
        .await
        .wrap_err_with(|| format!("POST to {BAKED_WEBHOOK}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        return Ok(body);
    }
    Err(eyre!(
        "worker rejected request: HTTP {} — {}",
        status.as_u16(),
        body.trim()
    ))
}
