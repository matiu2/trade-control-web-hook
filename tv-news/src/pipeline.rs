//! Top-level orchestration for `tv-news`.
//!
//! Phase-1 scaffold: read the chart's symbol and visible range via
//! tv-mcp, resolve the symbol against the `instrument-lookup` catalog
//! to learn which currencies the asset reacts to, and log the
//! resulting filter. Tasks #53 (multi-week event fetch) and #55
//! (filter + dedupe + draw) layer the actual side-effects on top.

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, eyre};
use instrument_lookup::Asset;
use tracing::info;
use trading_view::mcp::TvMcp;

use crate::args::Args;

/// Result of resolving the chart into a planning context — what
/// follow-up phases need before they can fetch and draw events.
#[derive(Debug)]
pub struct ChartContext {
    /// The chart's TradingView symbol as reported by tv-mcp (e.g.
    /// `"TRADENATION:EURUSD"`, `"OANDA:EUR_USD"`, or bare `"EURUSD"`).
    pub tv_symbol: String,
    /// The catalog entry the chart resolved to. Carries
    /// `news_currencies` for filtering.
    pub asset: &'static Asset,
    /// The chart's currently-visible window in UTC.
    pub visible_from: DateTime<Utc>,
    /// The end of the chart's currently-visible window in UTC.
    pub visible_to: DateTime<Utc>,
}

/// Entry point for `main.rs`. Returns the process exit code so the
/// binary can map a non-fatal "nothing to do" to 0 while still using
/// `?` for hard errors.
pub fn run(args: Args) -> Result<i32> {
    let mcp = build_mcp(&args);
    let ctx = read_chart_context(&mcp)?;

    info!(
        tv_symbol = %ctx.tv_symbol,
        asset_id = %ctx.asset.id,
        news_currencies = ?ctx.asset.news_currencies,
        visible_from = %ctx.visible_from,
        visible_to = %ctx.visible_to,
        "resolved chart context",
    );

    let currencies = filter_currencies(ctx.asset);
    info!(
        ?currencies,
        "news currencies in scope (incl. USD 3★ baseline)"
    );

    if args.dry_run {
        info!("dry-run: skipping event fetch and chart drawing");
        return Ok(0);
    }

    // Phase-2 (#53) and phase-3 (#55) work goes here. For now the
    // scaffold exits cleanly so the operator can verify the read path
    // against a live chart before the side-effect plumbing lands.
    info!("event fetch + draw not yet implemented (tasks #53 + #55)");
    Ok(0)
}

/// Build the tv-mcp wrapper, honouring `--tv-mcp-root` when supplied.
fn build_mcp(args: &Args) -> TvMcp {
    match args.tv_mcp_root.clone() {
        Some(root) => TvMcp::new(root),
        None => TvMcp::default(),
    }
}

/// Read chart state + range from tv-mcp and resolve the symbol into a
/// catalog entry. Hard-errors if the symbol isn't catalogued so the
/// operator gets a clear "add it to the overlay" hint rather than a
/// silent skip.
fn read_chart_context(mcp: &TvMcp) -> Result<ChartContext> {
    let state = mcp.get_state()?;
    let range = mcp.get_range()?;
    let (visible_from, visible_to) = range.visible_range.to_utc()?;

    let bare = strip_exchange(&state.symbol);
    let asset = instrument_lookup::resolve(bare)?.ok_or_else(|| {
        let hint = instrument_lookup::user_config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.config/instrument-lookup/mappings.toml".to_string());
        eyre!(
            "chart symbol {:?} is not in the instrument-lookup catalog. \
             Add an `[[asset]]` entry to {} to teach tv-news about it.",
            state.symbol,
            hint,
        )
    })?;

    Ok(ChartContext {
        tv_symbol: state.symbol.clone(),
        asset,
        visible_from,
        visible_to,
    })
}

/// Strip a `EXCHANGE:` prefix from a TV symbol if present. Mirrors the
/// helper in `tv-arm/src/instrument_resolution.rs` — when tv-news grows
/// a second consumer of this we should hoist the helper into
/// `instrument-lookup` itself.
fn strip_exchange(tv_symbol: &str) -> &str {
    match tv_symbol.split_once(':') {
        Some((_, sym)) => sym,
        None => tv_symbol,
    }
}

/// The set of forex-factory currencies whose 2★/3★ events should land
/// on the chart. Always includes USD so 3★ FOMC-class events show up
/// regardless of the asset's own news currencies.
///
/// Returns currencies in upper-case to match `EconomicEvent::currency`
/// shape from `forex-factory`. The dedupe / filter phase (#55) will
/// apply the per-currency star threshold (3★ for USD-only entries,
/// 2★+3★ for the asset's own currencies).
pub fn filter_currencies(asset: &Asset) -> Vec<String> {
    let mut out: Vec<String> = asset
        .news_currencies
        .iter()
        .map(|c| c.to_uppercase())
        .collect();
    let usd = "USD".to_string();
    if !out.iter().any(|c| c == &usd) {
        out.push(usd);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_exchange_handles_no_prefix() {
        assert_eq!(strip_exchange("EURUSD"), "EURUSD");
        assert_eq!(strip_exchange("TRADENATION:EURUSD"), "EURUSD");
        assert_eq!(strip_exchange("OANDA:EUR_USD"), "EUR_USD");
    }

    #[test]
    fn filter_currencies_includes_usd_baseline() {
        // EURUSD: asset already has USD, should not be duplicated.
        let eurusd = instrument_lookup::resolve("EURUSD")
            .expect("catalog ok")
            .expect("EURUSD exists");
        let cs = filter_currencies(eurusd);
        assert!(cs.iter().any(|c| c == "EUR"));
        let usd_count = cs.iter().filter(|c| **c == "USD").count();
        assert_eq!(usd_count, 1, "USD must be present exactly once");
    }

    #[test]
    fn filter_currencies_appends_usd_when_absent() {
        // SMI: CHF + EUR per catalog — USD should be appended.
        let smi = instrument_lookup::resolve("CH20")
            .or_else(|_| instrument_lookup::resolve("SMI"))
            .expect("catalog ok");
        if let Some(asset) = smi {
            let cs = filter_currencies(asset);
            assert!(
                cs.iter().any(|c| c == "USD"),
                "USD baseline missing: {cs:?}"
            );
        }
    }
}
