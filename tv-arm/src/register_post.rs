//! POST a signed `register` intent (carrying the whole [`TradePlan`]) to the
//! worker.
//!
//! This is tv-arm's first *direct* HTTP path to the worker. Until now the only
//! way tv-arm reached the worker was indirectly — TradingView delivered the
//! signed alert `message` when an alert fired. The server-side engine inverts
//! that: tv-arm folds the whole trade into one [`TradePlan`] and registers it
//! up front, so the worker can evaluate the conditions itself on its cron tick.
//!
//! The destination is the same baked-at-build-time webhook the TV alerts POST
//! to (`BAKED_WEBHOOK`, see [`crate::create_alerts`]), so a
//! `tv-arm-staging` binary registers against the staging worker with no env
//! var or flag — endpoint parity with the alert path is automatic.
//!
//! **Old + new run in parallel** (Stage F retires the alert path): a register
//! POST is *additive* to `create_alerts`, never a replacement. It is also
//! best-effort from the operator's point of view — a failed register is logged
//! and returned as an error to `run`, but the signed alert bundle is already on
//! disk and (if `--create-alerts`) armed on TV, so the trade is not lost.

use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr, eyre};

/// The worker endpoint, baked in at build time by `build.rs` from
/// `TRADE_CONTROL_WEBHOOK` — the same source as the TV alert `web_hook`. Each
/// per-environment binary embeds its own worker URL.
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
    let runtime =
        tokio::runtime::Runtime::new().wrap_err("starting tokio runtime for register POST")?;
    runtime.block_on(post_register(signed_body))
}

/// Async POST of the signed body. Factored out so it can be exercised against a
/// local mock in tests without the runtime bridge.
async fn post_register(signed_body: String) -> Result<()> {
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
        .wrap_err_with(|| format!("POST register to {BAKED_WEBHOOK}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        return Ok(());
    }
    Err(eyre!(
        "worker rejected register: HTTP {} — {}",
        status.as_u16(),
        body.trim()
    ))
}
