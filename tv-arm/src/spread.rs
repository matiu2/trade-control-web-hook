//! Live broker bid/ask spread read for M/W arming.
//!
//! The worker has no live spread at entry time (the Pine hook pushes
//! only mid OHLC), so the mid→bid/ask correction baked into the enter
//! intent needs the spread captured **at arm time**. This module is
//! that read.
//!
//! Two sources, picked by broker:
//!
//! - **OANDA** — the v20 `/pricing` endpoint (`get_pricing`) returns a
//!   bid/ask ladder; we take `best_ask - best_bid`. Needs the shared
//!   `OANDA_TOKEN` (or `OANDA_API_KEY`) bearer token and *any* account
//!   under it — the spread is account-agnostic, so we use the first
//!   account `get_accounts` returns.
//! - **TradeNation** — the unauthenticated `charts.finsatechnology.com`
//!   chart endpoint via `get_candles_bid_ask_by_name`, taking the
//!   latest 1-minute candle's `ask_close - bid_close`. A throwaway demo
//!   session (`TradeNationClient::new_demo`) supplies the name→market_id
//!   resolve; the bid/ask fetch itself is unauthenticated.
//!
//! There is **no operator override flag** — the spread is always read
//! live, and any failure (no token, network error, market closed, zero
//! or inverted spread) is a hard error that aborts the arm. A baked
//! stale or guessed spread would silently mis-size every M/W entry, so
//! we refuse to arm rather than fall back.

use color_eyre::eyre::{Context, Result, eyre};
use tracing::info;
use trade_control_conventions::Broker;

/// Read the live broker spread for `instrument` and express it in pips
/// (spread in price units / `pip_size`).
///
/// `instrument` is the broker-canonical symbol (`EUR_USD` for OANDA,
/// `EUR/USD` for TradeNation) — i.e. what `resolve_for_broker` already
/// produced. `pip_size` is the canonical catalog pip for the asset.
///
/// Hard-errors (never falls back) on any read failure or a
/// non-positive spread.
pub async fn read_spread_pips(broker: Broker, instrument: &str, pip_size: f64) -> Result<f64> {
    // Reject zero, negative, and NaN pip sizes (a NaN fails `> 0.0`).
    if pip_size.is_nan() || pip_size <= 0.0 {
        return Err(eyre!(
            "pip_size must be positive to convert spread to pips; got {pip_size}"
        ));
    }
    let (bid, ask) = match broker {
        Broker::Oanda => read_oanda_bid_ask(instrument).await?,
        Broker::TradeNation => read_tradenation_bid_ask(instrument).await?,
    };
    let spread_price = spread_from_bid_ask(bid, ask)?;
    let spread_pips = spread_price / pip_size;
    info!(
        broker = broker.as_str(),
        instrument, bid, ask, spread_price, pip_size, spread_pips, "live broker spread read",
    );
    Ok(spread_pips)
}

/// Validate a bid/ask pair and return the spread in price units. A
/// non-finite, zero, or inverted (`ask <= bid`) quote is a hard error —
/// it usually means the market is closed or the feed is stale, and a
/// degenerate spread must not be baked into an order.
fn spread_from_bid_ask(bid: f64, ask: f64) -> Result<f64> {
    if !bid.is_finite() || !ask.is_finite() {
        return Err(eyre!(
            "broker returned a non-finite quote: bid={bid}, ask={ask}"
        ));
    }
    let spread = ask - bid;
    if spread <= 0.0 {
        return Err(eyre!(
            "broker spread is non-positive (bid={bid}, ask={ask}) — market likely closed or \
             feed stale; refusing to bake a degenerate spread"
        ));
    }
    Ok(spread)
}

/// OANDA `/pricing` read. Token from `OANDA_TOKEN` (preferred) or
/// `OANDA_API_KEY`; account is the first one the token can see (spread
/// is account-independent).
async fn read_oanda_bid_ask(instrument: &str) -> Result<(f64, f64)> {
    use oanda_client::OandaClient;

    let token = std::env::var("OANDA_TOKEN")
        .or_else(|_| std::env::var("OANDA_API_KEY"))
        .map_err(|_| {
            eyre!(
                "no OANDA token in env — set OANDA_TOKEN (or OANDA_API_KEY) so tv-arm can read \
                 the live spread for an OANDA M/W arm"
            )
        })?;
    let client = OandaClient::new(token);
    let accounts = client
        .get_accounts()
        .await
        .wrap_err("list OANDA accounts for spread read")?;
    let account_id = accounts
        .first()
        .ok_or_else(|| eyre!("OANDA token has no accounts; cannot read pricing"))?;
    let pricing = client
        .get_pricing(account_id, &[instrument])
        .await
        .wrap_err_with(|| format!("read OANDA pricing for {instrument}"))?;
    let tick = pricing
        .prices
        .first()
        .ok_or_else(|| eyre!("OANDA pricing returned no tick for {instrument}"))?;
    let bid = tick
        .best_bid()
        .ok_or_else(|| eyre!("OANDA pricing for {instrument} has no bid"))?;
    let ask = tick
        .best_ask()
        .ok_or_else(|| eyre!("OANDA pricing for {instrument} has no ask"))?;
    Ok((bid, ask))
}

/// TradeNation chart read. A throwaway demo session resolves the
/// name→`market_id`; the bid/ask read itself (`latest_bid_ask`) hits
/// the unauthenticated `charts.finsatechnology.com` endpoint with a
/// fresh `reqwest::Client`, so it needs no session of its own.
async fn read_tradenation_bid_ask(instrument: &str) -> Result<(f64, f64)> {
    use tradenation_api::client::TradeNationClient;
    use tradenation_api::ohlcv::latest_bid_ask;

    let client = TradeNationClient::new_demo();
    let market = client
        .resolve_market(instrument)
        .await
        .wrap_err_with(|| format!("resolve TradeNation market for {instrument}"))?;
    let http = reqwest::Client::new();
    let (bid, ask) = latest_bid_ask(&http, market.market_id).await.map_err(|e| {
        eyre!(
            "read TradeNation bid/ask for {instrument} (market_id={}): {e}",
            market.market_id
        )
    })?;
    Ok((bid, ask))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_from_bid_ask_normal() {
        // 1.5-pip-ish EURUSD spread.
        let s = spread_from_bid_ask(1.10000, 1.10015).expect("ok");
        assert!((s - 0.00015).abs() < 1e-12);
    }

    #[test]
    fn spread_from_bid_ask_zero_is_error() {
        assert!(spread_from_bid_ask(1.1, 1.1).is_err());
    }

    #[test]
    fn spread_from_bid_ask_inverted_is_error() {
        // ask below bid — crossed/stale feed.
        assert!(spread_from_bid_ask(1.10020, 1.10000).is_err());
    }

    #[test]
    fn spread_from_bid_ask_nonfinite_is_error() {
        assert!(spread_from_bid_ask(f64::NAN, 1.1).is_err());
        assert!(spread_from_bid_ask(1.1, f64::INFINITY).is_err());
    }

    #[test]
    fn read_spread_pips_rejects_nonpositive_pip_size() {
        // pip_size <= 0 is caught before any network call.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        let err = rt
            .block_on(read_spread_pips(Broker::Oanda, "EUR_USD", 0.0))
            .expect_err("must reject zero pip_size");
        assert!(format!("{err}").contains("pip_size must be positive"));
    }

    // Conversion check: a 0.00015 price spread on a 0.0001 pip is 1.5
    // pips; on a 0.01 (JPY) pip the same numeric price spread is a much
    // larger pip count. We test the math via spread_from_bid_ask +
    // manual division here since the live read needs a broker.
    #[test]
    fn pip_conversion_scales_with_pip_size() {
        let spread = spread_from_bid_ask(1.10000, 1.10015).expect("ok");
        assert!((spread / 0.0001 - 1.5).abs() < 1e-9);
        // Same price spread against a JPY-scale pip.
        assert!((spread / 0.01 - 0.015).abs() < 1e-9);
    }
}
