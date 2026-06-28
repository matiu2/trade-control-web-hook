//! Resolve an instrument's market-hours no-entry windows for the replay.
//!
//! The live worker rejects/sweeps a resting order caught inside an instrument's
//! daily close→open session gap. Live, those windows are written to KV daily by
//! the `blackout_hours` cron, which calls TradeNation `market_info` and feeds the
//! Brisbane session ranges through the pure `core::windows_from_session` deriver.
//! The replay feeds the resulting windows to [`engine::sweep_reason`] so a
//! market-hours-blackout sweep is reconstructed the way the live worker would.
//!
//! # Same source as the worker — TradeNation `market_info`
//!
//! To stay in lockstep with production this calls the **same** TradeNation
//! endpoint the `blackout_hours` cron does — `resolve_market` to turn the
//! instrument name into a `market_id`, then `get_market_info` for that id — and
//! hands the Brisbane session ranges to the identical pure deriver
//! ([`windows_from_session`], shared in `core`). TradingView's charted-exchange
//! hours would diverge from the broker's CFD session, so they are deliberately
//! *not* used. See `src/cron/blackout_hours.rs` for the worker side.
//!
//! # OANDA: empty for now ("coming soon")
//!
//! The worker's cron skips OANDA-scoped instruments (no `market_info`
//! equivalent), so for parity the replay returns no windows for `--source
//! oanda`. OANDA venue-hours support is planned; when it lands it folds in here
//! the same way (resolve hours → `NoEntryWindow`s) and the engine seam is
//! unchanged.
//!
//! # Fail-soft, always
//!
//! Market hours are a post-mortem annotation, never a fill/exit decision, so any
//! miss — a login failure, an unresolvable market, a broker error, an
//! unparseable session — logs a `WARN` and returns an **empty** set. Empty
//! windows ⇒ `sweep_reason`'s blackout branch never fires (its fail-open), so
//! the replay behaves exactly as it did before this source was wired (the order
//! still reports `NEVER FILLED`, just without the blackout label).

use color_eyre::eyre::{Result, eyre};
use trade_control_core::intent::{Buffers, NoEntryWindow, windows_from_session};
use tradenation_api::{Session, get_market_info, login, login_demo, resolve_market};

use super::source::CandleSource;

/// Resolve `instrument`'s market-hours no-entry windows from the same source the
/// live worker uses, or an empty set (with a `WARN`) on any miss.
///
/// `instrument` is the broker-native name the candle pull resolves to (e.g.
/// `EUR/USD` for TradeNation). OANDA stays empty — the worker's `blackout_hours`
/// cron skips OANDA (no `market_info` equivalent); venue hours are coming soon.
pub async fn resolve_blackout_windows(
    source: CandleSource,
    instrument: &str,
) -> Vec<NoEntryWindow> {
    match source {
        CandleSource::TradeNation => match tradenation_windows(instrument).await {
            Ok(windows) => {
                tracing::info!(
                    "market-hours: {instrument} → {} blackout window(s) {windows:?}",
                    windows.len()
                );
                windows
            }
            Err(err) => {
                tracing::warn!(
                    "market-hours: could not resolve windows for {instrument}: {err:#}; \
                     blackout sweeps won't be reconstructed (the order still reports NEVER \
                     FILLED, just without the blackout label)."
                );
                Vec::new()
            }
        },
        CandleSource::Oanda => {
            tracing::warn!(
                "market-hours: OANDA venue hours not modelled yet (coming soon); {instrument} \
                 gets no blackout windows, matching the worker's TN-only cron."
            );
            Vec::new()
        }
    }
}

/// Resolve a TradeNation instrument's session into merged UTC blackout windows,
/// mirroring `src/cron/blackout_hours.rs::resolve_windows`. An empty `Vec` (a
/// 24h market with no close→open gap) is a valid success, not an error.
async fn tradenation_windows(instrument: &str) -> Result<Vec<NoEntryWindow>> {
    let session = tradenation_session().await?;
    let client = reqwest::Client::new();

    let market = resolve_market(&client, &session, instrument)
        .await
        .map_err(|e| eyre!("resolve_market({instrument}): {e}"))?;

    let info = get_market_info(&client, &session, market.market_id)
        .await
        .map_err(|e| {
            eyre!(
                "get_market_info({instrument}, id={}): {e}",
                market.market_id
            )
        })?;

    // The crate already converted the broker's London session to Brisbane
    // (DST-correct, anchored today). Hand the Brisbane (open, close) pairs to the
    // pure deriver — identical to the worker — which lands the UTC windows.
    let ranges: Vec<(String, String)> = info
        .trade_session
        .ranges
        .iter()
        .map(|r| (r.open_brisbane.clone(), r.close_brisbane.clone()))
        .collect();

    Ok(windows_from_session(&ranges, Buffers::default()))
}

/// Build a TradeNation [`Session`] for the read-only market-hours query, using
/// the same `TN_ACCOUNT_TYPE` convention as the candle pull
/// (`super::candles::tradenation_source`): `demo` (default) bootstraps a demo
/// session, `live` needs `TN_USERNAME` + `TN_PASSWORD`.
async fn tradenation_session() -> Result<Session> {
    let kind = std::env::var("TN_ACCOUNT_TYPE").unwrap_or_else(|_| "demo".to_string());
    match kind.to_ascii_lowercase().as_str() {
        "live" => {
            let user = std::env::var("TN_USERNAME")
                .map_err(|_| eyre!("TN_USERNAME not set (required for TN_ACCOUNT_TYPE=live)"))?;
            let pass = std::env::var("TN_PASSWORD")
                .map_err(|_| eyre!("TN_PASSWORD not set (required for TN_ACCOUNT_TYPE=live)"))?;
            login(&user, &pass)
                .await
                .map_err(|e| eyre!("TradeNation live login: {e}"))
        }
        "demo" => login_demo()
            .await
            .map_err(|e| eyre!("TradeNation demo login: {e}")),
        other => Err(eyre!(
            "TN_ACCOUNT_TYPE={other:?} not understood; use `demo` or `live`"
        )),
    }
}
