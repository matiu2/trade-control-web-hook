//! Pull the replay window straight from the current TradingView chart.
//!
//! The operator's workflow is: rewind TradingView replay-mode, arm a plan with
//! `tv-arm`, then scrub the chart forward to the end of the trade. At that point
//! the chart's *visible region* is exactly the window we want to replay, and the
//! chart's symbol + resolution are the instrument + granularity. So rather than
//! re-typing `--instrument/--granularity/--start/--end`, `replay-candles` reads
//! them off the live chart via the same `trading-view` MCP wrapper tv-arm uses.
//!
//! CLI flags remain authoritative overrides; this module only supplies the
//! values the operator didn't pass.

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, WrapErr, eyre};
use trading_view::mcp::TvMcp;

/// The window + instrument + granularity recovered from the live chart. Each
/// field mirrors a `replay-candles` flag; the caller layers any explicit flag
/// on top.
#[derive(Debug, Clone)]
pub struct TvDefaults {
    /// Bare TradingView symbol (exchange prefix stripped) — e.g. `EUR_CAD`,
    /// `EURUSD`. Fed through instrument-lookup downstream, same as a `--instrument`.
    pub instrument: String,
    /// Friendly granularity string (`1m`/`5m`/`15m`/`1h`/`4h`/`1d`) parsed from
    /// the chart resolution.
    pub granularity: String,
    /// Visible-region start (the replay rewind point).
    pub start: DateTime<Utc>,
    /// Visible-region end (where the operator scrubbed forward to).
    pub end: DateTime<Utc>,
}

/// Read instrument + granularity + visible window from the live TradingView
/// chart. Two MCP calls: `state` (symbol + resolution) and `range`
/// (visible window). Surfaces a clear error if TradingView's resolution isn't
/// one the engine can poll.
pub fn pull_defaults(mcp: &TvMcp) -> Result<TvDefaults> {
    let state = mcp.get_state().wrap_err("read TradingView chart state")?;
    let instrument = strip_exchange_prefix(&state.symbol).to_string();
    let granularity = resolution_to_friendly(&state.resolution)
        .ok_or_else(|| {
            eyre!(
                "TradingView chart resolution {:?} isn't a granularity the engine \
                 can replay (supported: 1m, 5m, 15m, 1h, 4h, 1d); set --granularity \
                 explicitly",
                state.resolution
            )
        })?
        .to_string();

    let range = mcp.get_range().wrap_err("read TradingView visible range")?;
    let (start, end) = range
        .visible_range
        .to_utc()
        .wrap_err("convert TradingView visible range to UTC")?;

    Ok(TvDefaults {
        instrument,
        granularity,
        start,
        end,
    })
}

/// Strip the `EXCHANGE:` prefix off a full TradingView symbol
/// (`OANDA:EURUSD` -> `EURUSD`, `TRADENATION:EUR_CAD` -> `EUR_CAD`). A symbol
/// with no prefix is returned unchanged.
fn strip_exchange_prefix(symbol: &str) -> &str {
    symbol.split_once(':').map(|(_, sym)| sym).unwrap_or(symbol)
}

/// Map a TradingView resolution code to the friendly granularity string the
/// CLI's [`super::granularity::parse`] understands. Mirrors tv-arm's
/// `resolution_to_granularity`; returns `None` for anything the engine can't
/// poll (sub-minute, weekly, etc.) so the caller rejects rather than guessing.
fn resolution_to_friendly(resolution: &str) -> Option<&'static str> {
    match resolution.trim() {
        "1" => Some("1m"),
        "5" => Some("5m"),
        "15" => Some("15m"),
        "60" => Some("1h"),
        "240" => Some("4h"),
        "D" | "1D" => Some("1d"),
        _ => None,
    }
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

    #[test]
    fn maps_known_resolutions() {
        assert_eq!(resolution_to_friendly("1"), Some("1m"));
        assert_eq!(resolution_to_friendly("5"), Some("5m"));
        assert_eq!(resolution_to_friendly("15"), Some("15m"));
        assert_eq!(resolution_to_friendly("60"), Some("1h"));
        assert_eq!(resolution_to_friendly("240"), Some("4h"));
        assert_eq!(resolution_to_friendly("D"), Some("1d"));
        assert_eq!(resolution_to_friendly(" 60 "), Some("1h"));
    }

    #[test]
    fn rejects_unsupported_resolutions() {
        assert_eq!(resolution_to_friendly("3"), None);
        assert_eq!(resolution_to_friendly("W"), None);
        assert_eq!(resolution_to_friendly(""), None);
    }

    /// The friendly strings this module emits must all round-trip through the
    /// CLI's own granularity parser — otherwise a TV-derived granularity would
    /// be rejected downstream.
    #[test]
    fn emitted_granularities_parse() {
        for res in ["1", "5", "15", "60", "240", "D"] {
            let friendly = resolution_to_friendly(res).expect("known resolution");
            assert!(
                super::super::granularity::parse(friendly).is_ok(),
                "friendly {friendly:?} from resolution {res:?} should parse"
            );
        }
    }
}
