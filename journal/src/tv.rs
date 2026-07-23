//! Loading a plan into the live TradingView chart via the Node-side
//! `tradingview-mcp-jackson` CLI. "Load" = navigate the chart: set the symbol,
//! set the timeframe, scroll to the setup's anchor time, then zoom out ~3× so
//! the whole setup is visible. It does **not** draw anything — the replay
//! `--annotate` path (on the Replay screen) owns drawing.
//!
//! Each step shells `node <root>/src/cli/index.js <cmd> …`. TradingView needs a
//! beat to catch up between commands, so we sleep ~1s between them (calibrated
//! interactively — without it the symbol/timeframe change races the scroll).
//!
//! Times are passed to `scroll`/`range` as **unix timestamps**, not date
//! strings: the Node side parses a bare date in the box's local TZ, but plan
//! times are UTC RFC3339 — a unix ts is unambiguous.

use std::path::PathBuf;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use chrono::DateTime;
use color_eyre::eyre::{Result, eyre};

/// The Node tv-mcp checkout. Matches `trading_view::mcp::DEFAULT_TV_MCP_ROOT`
/// and the hard-coded path in `replay-candles`. One-user tool, fine hard-coded.
const TV_MCP_ROOT: &str = "/home/matiu/Downloads/tradingview-mcp-jackson";

/// Bars either side of the anchor to show — `scroll` centres ±25 bars; we widen
/// to ±75 (~3× zoom-out) so the whole setup is in view.
const HALF_WINDOW_BARS: i64 = 75;

/// Pause between tv-mcp commands so TradingView can catch up.
const STEP_PAUSE: Duration = Duration::from_millis(1000);

/// Load a plan onto the live chart: symbol → timeframe → scroll(anchor) →
/// zoom-out. `instrument` is the plan's raw id (OANDA/TradeNation form),
/// `broker` its broker (`oanda`/`tradenation`) — which fixes the TradingView
/// *exchange prefix* so the right broker's chart loads — `granularity` its
/// `h1`/`m15`/… string, `anchor_utc` the RFC3339 time to centre on
/// (the plan's `armed_at`).
pub fn load_chart(
    instrument: &str,
    broker: &str,
    granularity: &str,
    anchor_utc: &str,
) -> Result<()> {
    let symbol = tv_symbol(instrument, broker)?;
    let resolution = tv_resolution(granularity)?;
    let anchor_ts = to_unix(anchor_utc)?;
    let secs_per_bar = resolution_secs(&resolution);
    let half = HALF_WINDOW_BARS * secs_per_bar;

    // 1. symbol, 2. timeframe — each needs a beat before the next.
    tv(&["symbol", &symbol])?;
    sleep(STEP_PAUSE);
    tv(&["timeframe", &resolution])?;
    sleep(STEP_PAUSE);
    // 3. scroll centres on the anchor AND loads the surrounding bars, so the
    //    following range call has bars to snap against.
    tv(&["scroll", &anchor_ts.to_string()])?;
    sleep(STEP_PAUSE);
    // 4. widen the visible window ~3× around the anchor.
    let from = (anchor_ts - half).to_string();
    let to = (anchor_ts + half).to_string();
    tv(&["range", "--from", &from, "--to", &to])?;
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
    let bare = [Broker::Oanda, Broker::TradeNation]
        .into_iter()
        .find_map(|b| {
            by_broker_symbol(b, instrument)
                .ok()
                .flatten()
                .and_then(|asset| asset.symbol_for(Broker::TradingView))
                .map(str::to_string)
        })
        .ok_or_else(|| eyre!("no TradingView symbol for instrument `{instrument}`"))?;

    match tv_exchange(broker) {
        Some(exchange) => Ok(format!("{exchange}:{bare}")),
        // Unknown/blank broker: fall back to the bare symbol (TV's default
        // exchange), preserving prior behaviour rather than failing.
        None => Ok(bare),
    }
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

/// Seconds per bar for a TradingView resolution string — mirrors the Node
/// `scrollToDate` mapping so our window matches its bar maths.
fn resolution_secs(resolution: &str) -> i64 {
    match resolution {
        "1D" => 86_400,
        "1W" => 604_800,
        mins => mins.parse::<i64>().map(|m| m * 60).unwrap_or(60),
    }
}

/// Parse an RFC3339 UTC instant to a unix timestamp (seconds).
fn to_unix(rfc3339: &str) -> Result<i64> {
    DateTime::parse_from_rfc3339(rfc3339)
        .map(|dt| dt.timestamp())
        .map_err(|e| eyre!("parse anchor time `{rfc3339}`: {e}"))
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
    fn resolution_secs_match_node() {
        assert_eq!(resolution_secs("15"), 900);
        assert_eq!(resolution_secs("60"), 3600);
        assert_eq!(resolution_secs("240"), 14_400);
        assert_eq!(resolution_secs("1D"), 86_400);
    }

    #[test]
    fn parses_utc_to_unix() {
        // 2026-07-23T00:00:00Z = 1784764800.
        assert_eq!(to_unix("2026-07-23T00:00:00Z").unwrap(), 1_784_764_800);
        assert!(to_unix("not-a-date").is_err());
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
    fn exchange_prefix_map() {
        assert_eq!(tv_exchange("tradenation"), Some("TRADENATION"));
        assert_eq!(tv_exchange("OANDA"), Some("OANDA"));
        assert_eq!(tv_exchange("mystery"), None);
    }
}
