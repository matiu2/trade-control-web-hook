//! Pull the replay defaults straight from the current TradingView chart.
//!
//! The operator's workflow is: arm a plan with `tv-arm`, rewind TradingView
//! replay-mode to **the start of the trade**, then just run `replay-candles`.
//! In replay mode the chart only renders bars up to the replay cursor, so the
//! chart's **last shown candle** (`bars_range.to`) is exactly the trade start —
//! that's the window start. The window *end* and the granularity come from the
//! signed plan (trade-expiry + `plan.granularity`), not the chart; this module
//! only supplies the start cursor, the symbol, and a fallback end.
//!
//! CLI flags remain authoritative overrides; this module only supplies the
//! values the operator didn't pass.

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, WrapErr};
use trading_view::mcp::TvMcp;

/// The chart-derived defaults the caller layers explicit flags / the plan on
/// top of: the symbol, the replay-cursor start, and a fallback end.
#[derive(Debug, Clone)]
pub struct TvDefaults {
    /// Bare TradingView symbol (exchange prefix stripped) — e.g. `EUR_CAD`,
    /// `EURUSD`. Fed through instrument-lookup downstream, same as a `--instrument`.
    pub instrument: String,
    /// The replay-cursor start: the chart's **last shown candle**
    /// (`bars_range.to`). In TV replay mode this is the trade start the
    /// operator rewound to.
    pub start: DateTime<Utc>,
    /// Fallback window end — the visible-region right edge (`visible_range.to`).
    /// Only used when the plan carries no trade-expiry rule; normally the end
    /// comes from the plan.
    pub fallback_end: DateTime<Utc>,
}

/// Read the symbol + replay-cursor start + fallback end from the live
/// TradingView chart. Two MCP calls: `state` (symbol) and `range` (the loaded
/// bar coverage + visible window). The granularity is no longer read here — it
/// comes from the plan.
pub fn pull_defaults(mcp: &TvMcp) -> Result<TvDefaults> {
    let state = mcp.get_state().wrap_err("read TradingView chart state")?;
    let instrument = strip_exchange_prefix(&state.symbol).to_string();

    let range = mcp.get_range().wrap_err("read TradingView visible range")?;
    // Start = last loaded bar (the replay cursor). `bars_range.to` is the right
    // edge of actually-rendered data, which in replay mode is the cursor.
    let (_, start) = range
        .bars_range
        .to_utc()
        .wrap_err("convert TradingView bars range to UTC")?;
    let (_, fallback_end) = range
        .visible_range
        .to_utc()
        .wrap_err("convert TradingView visible range to UTC")?;

    Ok(TvDefaults {
        instrument,
        start,
        fallback_end,
    })
}

/// Strip the `EXCHANGE:` prefix off a full TradingView symbol
/// (`OANDA:EURUSD` -> `EURUSD`, `TRADENATION:EUR_CAD` -> `EUR_CAD`). A symbol
/// with no prefix is returned unchanged.
fn strip_exchange_prefix(symbol: &str) -> &str {
    symbol.split_once(':').map(|(_, sym)| sym).unwrap_or(symbol)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_exchange_prefix() {
        assert_eq!(strip_exchange_prefix("OANDA:EURUSD"), "EURUSD");
        assert_eq!(strip_exchange_prefix("TRADENATION:EUR_CAD"), "EUR_CAD");
    }

    #[test]
    fn keeps_bare_symbol() {
        assert_eq!(strip_exchange_prefix("EURUSD"), "EURUSD");
    }
}
