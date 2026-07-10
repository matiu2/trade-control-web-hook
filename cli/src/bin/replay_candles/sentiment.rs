//! Recompute the news-sentiment verdict for a replay window.
//!
//! `tv-news` draws sentiment on a live chart; `tv-arm` bakes an arm-time
//! snapshot onto the plan. For a replay we recompute the **same** verdict
//! (identical `news_sentiment_tv` algorithm) as of the point the plan goes
//! live — so a journalled replay carries the sentiment picture the operator
//! would have seen, even for plans armed before the snapshot field existed.
//!
//! # "As of that point"
//!
//! The verdict is computed as of the plan's baked `armed_at` when present
//! (so it matches what `tv-arm` recorded), otherwise the replay window's
//! `start` — the moment the plan goes live in the replay. The lookback window
//! (24h, or back to Friday on Mondays) is anchored on that instant.
//!
//! # Fail-soft, always
//!
//! Sentiment is a post-mortem annotation, never a fill/exit decision, so any
//! miss — an uncatalogued instrument, a forex-factory fetch failure — logs a
//! `WARN` and returns `None`. The replay behaves exactly as before.

use chrono::{DateTime, Local, Utc};
use color_eyre::eyre::{Result, eyre};
use instrument_lookup::resolve;
use news_sentiment_tv::{analyze_sentiment, sentiment_lookback_start};
use tracing::warn;
use trade_control_cli::fetch_events_for_range;
use trade_control_core::plan_sentiment::PlanSentiment;
use trade_control_core::trade_plan::TradePlan;

/// Recompute the sentiment snapshot for `plan` as of `at` (the plan's
/// `armed_at` if it carries one, else the replay window `start`). Returns
/// `None` on any miss — never blocks the replay.
pub async fn resolve_replay_sentiment(
    plan: &TradePlan,
    start: DateTime<Utc>,
) -> Option<PlanSentiment> {
    let at = plan.armed_at.unwrap_or(start);
    match compute(&plan.instrument, at).await {
        Ok(snap) => snap,
        Err(e) => {
            warn!(error = %e, instrument = %plan.instrument, "replay sentiment: unavailable; skipping");
            None
        }
    }
}

/// Resolve the instrument's news currencies, fetch events over the lookback
/// window ending at `at`, and analyze. `Ok(None)` when the instrument has no
/// news currencies (nothing to score).
async fn compute(instrument: &str, at: DateTime<Utc>) -> Result<Option<PlanSentiment>> {
    let asset = resolve(instrument)
        .map_err(|e| eyre!("instrument-lookup overlay error resolving {instrument:?}: {e}"))?
        .ok_or_else(|| eyre!("instrument {instrument:?} not in the instrument-lookup catalog"))?;
    if asset.news_currencies.is_empty() {
        return Ok(None);
    }

    let at_local: DateTime<Local> = at.with_timezone(&Local);
    let window_start = sentiment_lookback_start(at_local);
    let events = fetch_events_for_range(window_start.to_utc(), at)
        .await
        .map_err(|e| eyre!("fetch forex-factory events for sentiment: {e}"))?;

    let analysis = analyze_sentiment(&asset.news_currencies, &events, at_local);
    Ok(Some(analysis.to_plan_sentiment()))
}
