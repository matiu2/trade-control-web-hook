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

use chrono::{DateTime, Duration, Local, Utc};
use color_eyre::eyre::{Result, eyre};
use instrument_lookup::Asset;
use tracing::{info, warn};
use trade_control_cli::{EconomicEvent, fetch_events_for_range};
use trading_view::drawings::Drawing;
use trading_view::mcp::TvMcp;

use crate::args::Args;
use crate::bucket::{EventBucket, bucket_events};
use crate::filter::{events_needing_drawing, filter_events};
use crate::label::is_news_label;
use crate::resolution::{DEFAULT_BAR_SECS, resolution_to_secs};
use crate::sentiment::{
    Confidence, CurrencySentiment, SentimentAnalysis, SentimentDirection, analyze_sentiment,
    sentiment_lookback_start,
};

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
    /// TradingView resolution string (`"15"`, `"60"`, `"D"`, ...).
    /// Drives the bar-bucketing so events in the same chart bar share
    /// a single drawing.
    pub resolution: String,
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

    let existing_anchors = collect_existing_news_anchors(&mcp)?;
    info!(
        existing_news_lines = existing_anchors.len(),
        "scanned chart for existing news drawings",
    );

    let tolerance = Duration::minutes(args.dedupe_tolerance_min);
    let to_draw = events_needing_drawing(filtered, &existing_anchors, tolerance);
    info!(
        events_to_draw = to_draw.len(),
        "after dedupe against existing chart drawings",
    );

    if to_draw.is_empty() {
        info!("nothing to draw — chart is already in sync");
    } else {
        let bar_secs = resolution_to_secs(&ctx.resolution).unwrap_or(DEFAULT_BAR_SECS);
        let buckets = bucket_events(to_draw, bar_secs);
        info!(
            resolution = %ctx.resolution,
            bar_secs,
            buckets = buckets.len(),
            "grouped events into chart-bar buckets",
        );

        if args.dry_run {
            log_planned_draws(&buckets);
            info!("dry-run: skipping chart drawing");
        } else {
            let drawn = draw_buckets(&mcp, &buckets)?;
            info!(drawn, "vertical lines landed on chart");
        }
    }

    if !args.no_sentiment {
        run_sentiment_phase(&ctx);
    }

    Ok(0)
}

/// Run a fresh fetch of recent events (covering the sentiment lookback
/// window, which is independent of the chart's visible range) and log
/// the per-currency + overall sentiment verdict. Logs errors instead of
/// returning them — the chart drawings are the primary output, and a
/// sentiment failure shouldn't fail the whole run.
fn run_sentiment_phase(ctx: &ChartContext) {
    let now_local = Local::now();
    let window_start = sentiment_lookback_start(now_local);

    let recent = match fetch_events_in_range(window_start.to_utc(), now_local.to_utc()) {
        Ok(evs) => evs,
        Err(e) => {
            warn!(error = %e, "sentiment: failed to fetch recent events; skipping");
            return;
        }
    };

    let news_currencies: Vec<String> = ctx
        .asset
        .news_currencies
        .iter()
        .map(|c| c.to_uppercase())
        .collect();

    let analysis = analyze_sentiment(&news_currencies, &recent, now_local);
    log_sentiment(&ctx.asset.id, &news_currencies, &analysis);
}

fn fetch_events_in_range(from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<EconomicEvent>> {
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| eyre!("starting tokio runtime for sentiment fetch: {e}"))?;
    runtime.block_on(fetch_events_for_range(from, to))
}

fn log_sentiment(asset_id: &str, news_currencies: &[String], a: &SentimentAnalysis) {
    let overall = direction_str(a.overall_direction);
    let conf = confidence_str(a.confidence);
    let total_events: usize = a
        .currency_sentiments
        .values()
        .map(|cs| cs.events.len())
        .sum();

    info!(
        asset = %asset_id,
        direction = %overall,
        confidence = %conf,
        events = total_events,
        period_start = %a.period_start,
        period_end = %a.period_end,
        "sentiment verdict",
    );

    for ccy in news_currencies {
        match a.currency_sentiments.get(ccy) {
            Some(cs) => log_currency_sentiment(cs),
            None => info!(currency = %ccy, "  no released events in lookback window"),
        }
    }
}

fn log_currency_sentiment(cs: &CurrencySentiment) {
    info!(
        currency = %cs.currency,
        direction = %direction_str(cs.direction),
        net_score = cs.net_score(),
        bullish = cs.bullish_score,
        bearish = cs.bearish_score,
        events = cs.events.len(),
        "  per-currency",
    );
    for ev in &cs.events {
        info!(
            event = %ev.event_name,
            direction = %direction_str(ev.direction),
            reason = %ev.reason,
            "    event",
        );
    }
}

fn direction_str(d: SentimentDirection) -> &'static str {
    match d {
        SentimentDirection::Bullish => "bullish",
        SentimentDirection::Bearish => "bearish",
        SentimentDirection::Neutral => "neutral",
    }
}

fn confidence_str(c: Confidence) -> &'static str {
    match c {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    }
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
/// tv-news event marker (per [`is_news_label`]) and collect their
/// anchor timestamps in unix seconds. Used to dedupe re-runs.
///
/// Drawings that fail to fetch are logged and skipped — a stale id in
/// the `draw list` response shouldn't fail the whole run, since the
/// worst case is a duplicate line on a transient race.
fn collect_existing_news_anchors(mcp: &TvMcp) -> Result<Vec<i64>> {
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
        if !is_news_label(drawing.label()) {
            continue;
        }
        if let Some(p) = drawing.points.first() {
            out.push(p.time);
        }
    }
    Ok(out)
}

/// Log the buckets we'd draw under `--dry-run` so the operator can
/// verify the plan before re-running for real.
fn log_planned_draws(buckets: &[EventBucket]) {
    for b in buckets {
        info!(
            anchor = %b.anchor(),
            event_count = b.events.len(),
            label = %b.label(),
            "would draw",
        );
    }
}

/// Draw one labelled vertical line per bucket at the bucket's anchor
/// time. Returns the count of lines that landed successfully. tv-mcp
/// errors short-circuit so the operator can re-run after fixing the
/// cause rather than ending up with a half-drawn chart.
fn draw_buckets(mcp: &TvMcp, buckets: &[EventBucket]) -> Result<usize> {
    let mut drawn = 0usize;
    for b in buckets {
        let anchor = b.anchor();
        let label = b.label();
        // tv-mcp wants a price; vertical lines ignore it for evaluation
        // but require something parseable. Use 1.0 — matches tv-arm's
        // auto-draw helper.
        let s = mcp.draw_vertical_line(anchor.timestamp(), 1.0, &label)?;
        if !s.success {
            return Err(eyre!(
                "tv-mcp draw vertical_line failed at {}: {}",
                anchor,
                s.error.as_deref().unwrap_or("(no message)"),
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
        resolution: state.resolution.clone(),
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
