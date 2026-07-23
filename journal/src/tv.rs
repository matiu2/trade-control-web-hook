//! Loading a plan into the live TradingView chart via the Node-side
//! `tradingview-mcp-jackson` CLI. "Load" = set the chart's symbol and timeframe
//! **only** — the operator scrolls/zooms to the setup manually. It does **not**
//! scroll to the anchor, set a visible range, or draw anything — the replay
//! `--annotate` path (on the Replay screen) owns drawing.
//!
//! Each step shells `node <root>/src/cli/index.js <cmd> …`. TradingView needs a
//! beat to catch up between commands, so we sleep ~1s between the symbol and
//! timeframe commands (calibrated interactively — without it the symbol change
//! races the timeframe change).

use std::path::PathBuf;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use color_eyre::eyre::{Result, eyre};

/// The Node tv-mcp checkout. Matches `trading_view::mcp::DEFAULT_TV_MCP_ROOT`
/// and the hard-coded path in `replay-candles`. One-user tool, fine hard-coded.
const TV_MCP_ROOT: &str = "/home/matiu/Downloads/tradingview-mcp-jackson";

/// Pause between tv-mcp commands so TradingView can catch up.
const STEP_PAUSE: Duration = Duration::from_millis(1000);

/// Load a plan onto the live chart: set the symbol and timeframe **only**. The
/// operator scrolls/zooms to the setup manually — we deliberately do **not**
/// scroll to the anchor or set a visible range. `instrument` is the plan's raw
/// id (OANDA/TradeNation form), `broker` its broker (`oanda`/`tradenation`) —
/// which fixes the TradingView *exchange prefix* so the right broker's chart
/// loads — `granularity` its `h1`/`m15`/… string.
pub fn load_chart(instrument: &str, broker: &str, granularity: &str) -> Result<()> {
    let symbol = tv_symbol(instrument, broker)?;
    let resolution = tv_resolution(granularity)?;

    // 1. symbol, 2. timeframe — the symbol change needs a beat before the
    //    timeframe change or the two race.
    tv(&["symbol", &symbol])?;
    sleep(STEP_PAUSE);
    tv(&["timeframe", &resolution])?;
    Ok(())
}

/// Resolve a plan instrument id to a **fully-qualified** TradingView symbol
/// (`EXCHANGE:SYMBOL`, e.g. `TRADENATION:AUDCHF`). instrument-lookup returns the
/// bare symbol (`AUDCHF`); without the exchange prefix TradingView picks its own
/// default exchange (OANDA for FX), which loaded the *wrong broker's* chart for
/// a TradeNation plan. So we prepend the exchange for the plan's actual broker.
fn tv_symbol(instrument: &str, broker: &str) -> Result<String> {
    use instrument_lookup::{Broker, by_broker_symbol};
    // Resolve the bare TV symbol, trying both broker views of the raw id.
    // Some catalog entries (many FX crosses, e.g. AUD/SGD) carry an OANDA/TN
    // symbol but a blank `tradingview` field, so `symbol_for(TradingView)`
    // returns None. TradingView symbols are just the raw id with separators
    // stripped, so fall back to that rather than failing: `AUD_SGD` → `AUDSGD`,
    // `AUD/SGD` → `AUDSGD`. Keeps any unknown FX instrument loadable.
    let bare = [Broker::Oanda, Broker::TradeNation]
        .into_iter()
        .find_map(|b| {
            by_broker_symbol(b, instrument)
                .ok()
                .flatten()
                .and_then(|asset| asset.symbol_for(Broker::TradingView))
                .map(str::to_string)
        })
        .unwrap_or_else(|| strip_separators(instrument));

    match tv_exchange(broker) {
        Some(exchange) => Ok(format!("{exchange}:{bare}")),
        // Unknown/blank broker: fall back to the bare symbol (TV's default
        // exchange), preserving prior behaviour rather than failing.
        None => Ok(bare),
    }
}

/// Turn a raw broker instrument id into a bare TradingView symbol by dropping
/// the separators brokers use but TradingView doesn't: `AUD_SGD` (OANDA form)
/// and `AUD/SGD` (TradeNation form) both → `AUDSGD`. Used as the last-resort
/// fallback when instrument-lookup has no TradingView symbol for the asset.
fn strip_separators(instrument: &str) -> String {
    instrument
        .chars()
        .filter(|c| !matches!(c, '_' | '/' | ' '))
        .collect()
}

/// The TradingView exchange prefix for a plan broker. `None` for an
/// unknown/blank broker (caller falls back to a bare symbol).
fn tv_exchange(broker: &str) -> Option<&'static str> {
    match broker.to_ascii_lowercase().as_str() {
        "tradenation" => Some("TRADENATION"),
        "oanda" => Some("OANDA"),
        _ => None,
    }
}

/// Map a plan granularity (`m1`/`m15`/`h1`/`h4`/`d`) to a TradingView
/// resolution string (`1`/`15`/`60`/`240`/`1D`).
fn tv_resolution(granularity: &str) -> Result<String> {
    let g = granularity.to_ascii_lowercase();
    let res = match g.as_str() {
        "m1" => "1",
        "m5" => "5",
        "m15" => "15",
        "m30" => "30",
        "h1" => "60",
        "h4" => "240",
        "d" | "d1" | "1d" => "1D",
        "w" | "w1" | "1w" => "1W",
        other => return Err(eyre!("unknown granularity `{other}`")),
    };
    Ok(res.to_string())
}

/// The tv-mcp CLI entrypoint.
fn cli_path() -> PathBuf {
    PathBuf::from(TV_MCP_ROOT).join("src/cli/index.js")
}

/// Shell one tv-mcp command (`node <cli> <args>`), surfacing stderr on failure.
fn tv(args: &[&str]) -> Result<()> {
    let cli = cli_path();
    let out = Command::new("node")
        .arg(&cli)
        .args(args)
        .output()
        .map_err(|e| eyre!("failed to spawn `node {}`: {e}", cli.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(eyre!(
            "tv-mcp `{}` failed ({}): {}",
            args.join(" "),
            out.status,
            stderr.trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_granularities() {
        assert_eq!(tv_resolution("m15").unwrap(), "15");
        assert_eq!(tv_resolution("h1").unwrap(), "60");
        assert_eq!(tv_resolution("h4").unwrap(), "240");
        assert_eq!(tv_resolution("d").unwrap(), "1D");
        assert!(tv_resolution("nonsense").is_err());
    }

    #[test]
    fn resolves_symbol_with_broker_exchange_prefix() {
        // A TradeNation plan → TRADENATION: prefix (the bug that loaded OANDA).
        assert_eq!(
            tv_symbol("AUD/CHF", "tradenation").unwrap(),
            "TRADENATION:AUDCHF"
        );
        // An OANDA plan → OANDA: prefix.
        assert_eq!(tv_symbol("GBP/USD", "oanda").unwrap(), "OANDA:GBPUSD");
        // Unknown broker → bare symbol (TV's default exchange), no failure.
        assert_eq!(tv_symbol("GBP/USD", "").unwrap(), "GBPUSD");
    }

    #[test]
    fn resolves_symbol_when_catalog_lacks_tv_field() {
        // AUD/SGD is in instrument-lookup with an OANDA symbol but a blank
        // `tradingview` field, so `symbol_for(TradingView)` is None. The
        // fallback strips the OANDA underscore → OANDA:AUDSGD (not AUD_SGD).
        assert_eq!(tv_symbol("AUD_SGD", "oanda").unwrap(), "OANDA:AUDSGD");
    }

    #[test]
    fn strips_broker_separators() {
        assert_eq!(strip_separators("AUD_SGD"), "AUDSGD");
        assert_eq!(strip_separators("AUD/SGD"), "AUDSGD");
        assert_eq!(strip_separators("Spot Gold"), "SpotGold");
        assert_eq!(strip_separators("EURUSD"), "EURUSD");
    }

    #[test]
    fn exchange_prefix_map() {
        assert_eq!(tv_exchange("tradenation"), Some("TRADENATION"));
        assert_eq!(tv_exchange("OANDA"), Some("OANDA"));
        assert_eq!(tv_exchange("mystery"), None);
    }
}
