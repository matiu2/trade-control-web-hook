//! Top-level orchestration for `tv-news`.
//!
//! 1. tv-mcp `state` + `range` → chart symbol + visible window.
//! 2. `instrument-lookup` → asset → news currencies.
//! 3. `cli::fetch_events_for_range` → forex-factory events spanning
//!    the visible window.
//! 4. [`crate::filter::filter_events`] → 2★+ for asset currencies,
//!    3★ for USD baseline.
//! 5. [`crate::filter::events_needing_drawing`] → drop events already
//!    annotated on the chart within ±tolerance.
//! 6. tv-mcp `draw vertical_line` × 2 per surviving event.

use chrono::{DateTime, Duration, Utc};
use color_eyre::eyre::{Result, eyre};
use instrument_lookup::Asset;
use tracing::{info, warn};
use trade_control_cli::{EconomicEvent, fetch_events_for_range};
use trade_control_conventions::{NEWS_START_LABELS, matches};
use trading_view::drawings::Drawing;
use trading_view::mcp::TvMcp;

use crate::args::Args;
use crate::filter::{events_needing_drawing, filter_events, news_window};

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

    let raw_events = fetch_events(&ctx)?;
    info!(
        events_fetched = raw_events.len(),
        "fetched forex-factory events spanning the visible window",
    );

    let baseline = vec!["USD".to_string()];
    let news_ccys: Vec<String> = ctx
        .asset
        .news_currencies
        .iter()
        .map(|c| c.to_uppercase())
        .collect();
    let filtered = filter_events(&raw_events, &news_ccys, &baseline);
    info!(
        events_kept = filtered.len(),
        "applied currency + impact filter",
    );

    let existing_starts = collect_existing_news_starts(&mcp)?;
    info!(
        existing_news_starts = existing_starts.len(),
        "scanned chart for existing news-start drawings",
    );

    let tolerance = Duration::minutes(args.dedupe_tolerance_min);
    let to_draw = events_needing_drawing(filtered, &existing_starts, tolerance);
    info!(
        events_to_draw = to_draw.len(),
        "after dedupe against existing chart drawings",
    );

    if to_draw.is_empty() {
        info!("nothing to draw — chart is already in sync");
        return Ok(0);
    }

    if args.dry_run {
        log_planned_draws(&to_draw, Duration::minutes(args.news_window_min));
        info!("dry-run: skipping chart drawing");
        return Ok(0);
    }

    let window = Duration::minutes(args.news_window_min);
    let drawn = draw_events(&mcp, &to_draw, window)?;
    info!(drawn, "vertical-line pairs landed on chart");
    Ok(0)
}

/// Run the multi-week forex-factory fetch on a fresh tokio runtime.
/// Keeps the rest of tv-news sync so the binary doesn't have to be
/// `#[tokio::main]` — matches the same pattern `cli::run_calendar_bars`
/// uses.
fn fetch_events(ctx: &ChartContext) -> Result<Vec<EconomicEvent>> {
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| eyre!("starting tokio runtime for forex-factory fetch: {e}"))?;
    runtime.block_on(fetch_events_for_range(ctx.visible_from, ctx.visible_to))
}

/// Scan the chart for vertical-line drawings whose label looks like a
/// `news-start` marker (per [`NEWS_START_LABELS`]) and collect their
/// anchor timestamps in unix seconds. Used to dedupe re-runs.
///
/// Drawings that fail to fetch are logged and skipped — a stale id in
/// the `draw list` response shouldn't fail the whole run, since the
/// worst case is a duplicate line on a transient race.
fn collect_existing_news_starts(mcp: &TvMcp) -> Result<Vec<i64>> {
    let stubs = mcp.list_drawings()?;
    let mut out = Vec::new();
    for stub in stubs {
        if stub.name != "vertical_line" {
            continue;
        }
        let drawing: Drawing = match mcp.get_drawing(&stub.id) {
            Ok(d) => d,
            Err(e) => {
                warn!(id = %stub.id, error = %e, "could not fetch drawing — skipping");
                continue;
            }
        };
        if !matches(drawing.label(), NEWS_START_LABELS) {
            continue;
        }
        if let Some(p) = drawing.points.first() {
            out.push(p.time);
        }
    }
    Ok(out)
}

/// Log the events we'd draw under `--dry-run` so the operator can
/// verify the plan before re-running for real.
fn log_planned_draws(events: &[EconomicEvent], window: Duration) {
    for ev in events {
        let (start, end) = news_window(ev, window);
        info!(
            event = %ev.name,
            currency = %ev.currency,
            impact = ?ev.impact,
            news_start = %start,
            news_end = %end,
            "would draw",
        );
    }
}

/// Draw one `news-start`/`news-end` vertical-line pair per event.
/// Returns the count of pairs that landed successfully. tv-mcp errors
/// short-circuit so the operator can re-run after fixing the cause
/// rather than ending up with a half-drawn chart.
fn draw_events(mcp: &TvMcp, events: &[EconomicEvent], window: Duration) -> Result<usize> {
    let mut drawn = 0usize;
    for ev in events {
        let (start, end) = news_window(ev, window);
        // tv-mcp wants a price; vertical lines ignore it for evaluation
        // but require something parseable. Use 1.0 — matches tv-arm's
        // auto-draw helper.
        let s = mcp.draw_vertical_line(start.timestamp(), 1.0, "news-start")?;
        if !s.success {
            return Err(eyre!(
                "tv-mcp draw news-start failed for {}: {}",
                ev.name,
                s.error.as_deref().unwrap_or("(no message)"),
            ));
        }
        let e = mcp.draw_vertical_line(end.timestamp(), 1.0, "news-end")?;
        if !e.success {
            return Err(eyre!(
                "tv-mcp draw news-end failed for {}: {}",
                ev.name,
                e.error.as_deref().unwrap_or("(no message)"),
            ));
        }
        drawn += 1;
    }
    Ok(drawn)
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
