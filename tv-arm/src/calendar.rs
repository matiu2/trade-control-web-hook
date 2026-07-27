//! Turning the economic calendar into the arm's blackout / news windows.
//!
//! Extracted from `pipeline.rs` unchanged.
//!
//! ## The scope range is the trade's lifetime, not the chart's view
//!
//! Windows are filtered to `[cursor, trade-expiry]`:
//!
//! - the **left** edge is the arm cursor (`--start` when given, else the last
//!   loaded bar) — so only news at or after "live now" matters, independent of
//!   how far left the chart happens to be scrolled;
//! - the **right** edge is the trade-expiry vertical — only news the open trade
//!   could still run into is considered.
//!
//! A missing or unparseable expiry collapses the range to **empty** rather than
//! fetching across all of time. That is deliberate: the absent expiry drawing is
//! a hard error from `check_required` moments later, and fetching a decade of
//! calendar first would just be slow before failing.

use chrono::{DateTime, TimeZone, Utc};
use color_eyre::eyre::{Context, Result, eyre};
use tracing::info;
use trade_control_cli as cli;

use crate::instrument_resolution::ResolvedInstrument;
use crate::news_marker::NewsMarker;
use crate::news_window::NewsWindow;
use crate::plan_geometry::PlanGeometry;
use crate::timeframe::infer_calendar_timeframe;

/// Parse the trade-expiry timestamp from the classified drawings.
/// Used both as the hard expiry for the trade bundle and (when known
/// pre-auto-draw) as the lookahead horizon for calendar bars so the
/// auto-draw covers the trade's full lifetime instead of just the
/// next H1+ buffer window.
pub fn read_trade_expiry(geom: &PlanGeometry) -> Result<DateTime<Utc>> {
    let expiry_unix = geom
        .trade_expiry_epoch
        .ok_or_else(|| eyre!("missing trade_expiry"))?;
    Utc.timestamp_opt(expiry_unix, 0)
        .single()
        .ok_or_else(|| eyre!("invalid trade_expiry timestamp {expiry_unix}"))
}

/// Compute the calendar news-scope range `[cursor, expiry]` in unix seconds.
///
/// Left edge is the cursor (`--start` when given, else the last loaded bar) —
/// so the scope is the trade's own lifetime, not the chart's visible area, and
/// scrolling the chart doesn't change which news is considered. Right edge is
/// the trade-expiry vertical; with no resolved expiry it collapses to the
/// cursor, giving an empty range (`calendar_windows` then returns no windows)
/// rather than fetching across all of time.
pub fn calendar_scope_range(cursor_unix: i64, expiry_hint: Option<DateTime<Utc>>) -> (i64, i64) {
    (
        cursor_unix,
        expiry_hint.map(|e| e.timestamp()).unwrap_or(cursor_unix),
    )
}

/// Resolve blackout (pause) and news windows from the economic calendar over
/// the trade's own lifetime `[from, to]`, at real event-minute precision.
///
/// `range` is `[cursor, trade-expiry]` in unix seconds — the cursor (`--start`
/// or the last loaded bar) as the left edge, the trade-expiry vertical as the
/// right. It is used **verbatim**: the news filter is bounded to the trade's
/// lifetime, NOT the chart's visible area, so scrolling the chart left/right no
/// longer changes which events are considered. An empty or reversed range
/// (`to <= from`, e.g. a missing expiry) yields no windows.
///
/// Returns `(blackout_windows, news_windows, markers)`. Each kept event yields a
/// **blackout** window `[event − before, event]` (no new entries in the run-up),
/// a **news** window `[event, event + after]` (post-release), and a cosmetic
/// **marker** (currency + stars + event minute) carrying the event detail the
/// windows drop — used to draw the cosmetic armed-news lines (default on, opt
/// out with `--skip-calendar-bars`). `before` / `after` default
/// to the chart timeframe's buffers, overridden per-run by `--news-before-hours`
/// / `--news-after-hours` when set.
///
/// No chart lines are drawn and nothing is read back — the returned windows are
/// pushed straight into `Roles`, preserving the true event minute (e.g. a 14:30
/// event on an H1 chart) instead of snapping it to a bar boundary.
pub fn calendar_windows(
    resolution: &str,
    resolved: &ResolvedInstrument,
    range: (i64, i64),
    before_hours: Option<f64>,
    after_hours: Option<f64>,
) -> Result<(Vec<NewsWindow>, Vec<NewsWindow>, Vec<NewsMarker>)> {
    let timeframe = infer_calendar_timeframe(resolution).ok_or_else(|| {
        eyre!("chart resolution {resolution:?} is below 15m; calendar bars skipped")
    })?;
    let window_start = Utc
        .timestamp_opt(range.0, 0)
        .single()
        .ok_or_else(|| eyre!("invalid calendar-range start {}", range.0))?;
    let lookahead_end = Utc
        .timestamp_opt(range.1, 0)
        .single()
        .ok_or_else(|| eyre!("invalid calendar-range end {}", range.1))?;
    if lookahead_end <= window_start {
        info!(
            window_start = %window_start.to_rfc3339(),
            lookahead_end = %lookahead_end.to_rfc3339(),
            "calendar range is empty (to <= from) — no calendar windows",
        );
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }
    // Synthesise the tcm Instrument straight from the catalog Asset so non-FX
    // assets (SMI, gold, indices) get correct news-currency exposure without
    // the FX-only cli::parse_instrument path.
    let instrument_parsed =
        crate::instrument_resolution::synthesize_calendar_instrument(resolved.asset);
    let runtime = tokio::runtime::Runtime::new().context("starting tokio runtime")?;
    let events = runtime
        .block_on(cli::fetch_events_for_range(window_start, lookahead_end))
        .wrap_err("fetch_events_for_range")?;
    let inputs = cli::PlanInputs {
        trade_id: String::new(),
        instrument: resolved.broker_symbol.clone(),
        account: String::new(),
        broker: cli::BrokerKind::Oanda,
    };
    let plan = cli::plan_calendar_bars_within(
        &events,
        &instrument_parsed,
        timeframe.into(),
        window_start,
        lookahead_end,
        &inputs,
    )
    .wrap_err("plan_calendar_bars_within")?;

    // Per-run buffer overrides. When a flag is absent, keep the planner's
    // timeframe-derived spec boundary; when set, recompute from the event time.
    let before = before_hours.map(hours_to_duration).transpose()?;
    let after = after_hours.map(hours_to_duration).transpose()?;
    let mut blackout = Vec::with_capacity(plan.rows.len());
    let mut news = Vec::with_capacity(plan.rows.len());
    let mut markers = Vec::with_capacity(plan.rows.len());
    for row in &plan.rows {
        let pause_start = match before {
            Some(d) => row.event_time - d,
            None => row.pause_spec.start_time,
        };
        let news_end = match after {
            Some(d) => row.event_time + d,
            None => row.news_spec.end_time,
        };
        blackout.push(NewsWindow::new(pause_start, row.event_time));
        news.push(NewsWindow::new(row.event_time, news_end));
        // Cosmetic marker: same kept row, carrying the currency/impact the
        // windows discard. Drawn (opt-in) at the real event minute.
        markers.push(NewsMarker::new(&row.currency, row.impact, row.event_time));
    }
    info!(
        events_fetched = events.len(),
        events_kept = plan.rows.len(),
        blackout_windows = blackout.len(),
        news_windows = news.len(),
        window_start = %window_start.to_rfc3339(),
        lookahead_end = %lookahead_end.to_rfc3339(),
        "calendar windows resolved",
    );
    Ok((blackout, news, markers))
}

/// Convert a fractional-hours buffer (e.g. `0.5` = 30 min) to a `Duration`.
/// Negative values are a hard error — a buffer can't run backwards.
pub fn hours_to_duration(hours: f64) -> Result<chrono::Duration> {
    if hours < 0.0 || !hours.is_finite() {
        return Err(eyre!(
            "news buffer hours must be finite and >= 0, got {hours}"
        ));
    }
    let secs = (hours * 3600.0).round() as i64;
    chrono::Duration::try_seconds(secs)
        .ok_or_else(|| eyre!("news buffer {hours}h is out of representable range"))
}
