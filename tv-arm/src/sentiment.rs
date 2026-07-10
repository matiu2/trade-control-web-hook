//! Arm-time news-sentiment snapshot.
//!
//! When `tv-arm` arms a plan it computes the current news-sentiment verdict
//! (the same one `tv-news` logs) as of the effective arm time, prints a short
//! human summary, and returns a lean [`PlanSentiment`] to bake onto the plan
//! for after-the-fact journalling. The worker/engine never reads it.
//!
//! Everything here is **fail-soft**: a forex-factory fetch failure logs a
//! warning and yields `None` — arming must never block on news.

use chrono::{DateTime, Local, Utc};
use news_sentiment_tv::{
    CurrencySentiment, SentimentAnalysis, analyze_sentiment, confidence_str, direction_str,
    sentiment_lookback_start,
};
use tracing::{info, warn};
use trade_control_core::plan_sentiment::PlanSentiment;

/// Compute the arm-time sentiment snapshot for `instrument` (identified by its
/// `news_currencies`) as of `armed_at`, print a summary, and return the lean
/// [`PlanSentiment`] to bake onto the plan. Returns `None` on any fetch/analysis
/// failure — arming continues regardless.
pub fn arm_time_sentiment(
    asset_id: &str,
    news_currencies: &[String],
    armed_at: DateTime<Utc>,
) -> Option<PlanSentiment> {
    if news_currencies.is_empty() {
        info!(asset = %asset_id, "sentiment: asset has no news currencies; skipping snapshot");
        return None;
    }
    let at_local = armed_at.with_timezone(&Local);
    let window_start = sentiment_lookback_start(at_local);

    let events = match fetch_events(window_start.to_utc(), armed_at) {
        Ok(evs) => evs,
        Err(e) => {
            warn!(error = %e, "sentiment: failed to fetch events; no snapshot baked");
            return None;
        }
    };

    let analysis = analyze_sentiment(news_currencies, &events, at_local);
    print_summary(asset_id, news_currencies, &analysis);
    Some(analysis.to_plan_sentiment())
}

/// Fetch forex-factory events for the lookback window on a fresh tokio runtime,
/// keeping the surrounding pipeline synchronous (same trick `tv-news` uses).
fn fetch_events(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> color_eyre::Result<Vec<trade_control_cli::EconomicEvent>> {
    use color_eyre::eyre::eyre;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| eyre!("starting tokio runtime for sentiment fetch: {e}"))?;
    runtime.block_on(trade_control_cli::fetch_events_for_range(from, to))
}

/// Print a short human summary of the arm-time sentiment verdict — the same
/// information `tv-news` logs, surfaced at arm time so the operator sees it.
fn print_summary(asset_id: &str, news_currencies: &[String], a: &SentimentAnalysis) {
    let total_events: usize = a
        .currency_sentiments
        .values()
        .map(|cs| cs.events.len())
        .sum();
    info!(
        asset = %asset_id,
        direction = %direction_str(a.overall_direction),
        confidence = %confidence_str(a.confidence),
        events = total_events,
        period_start = %a.period_start,
        period_end = %a.period_end,
        "arm-time sentiment verdict",
    );
    for ccy in news_currencies {
        match a.currency_sentiments.get(&ccy.to_uppercase()) {
            Some(cs) => print_currency(cs),
            None => info!(currency = %ccy, "  no released events in lookback window"),
        }
    }
}

fn print_currency(cs: &CurrencySentiment) {
    info!(
        currency = %cs.currency,
        direction = %direction_str(cs.direction),
        net_score = cs.net_score(),
        events = cs.events.len(),
        "  per-currency",
    );
    for ev in &cs.events {
        info!(event = %ev.event_name, reason = %ev.reason, "    event");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn empty_currencies_yields_none() {
        let armed = Utc.with_ymd_and_hms(2026, 1, 23, 14, 0, 0).unwrap();
        assert!(arm_time_sentiment("EUR_USD", &[], armed).is_none());
    }
}
