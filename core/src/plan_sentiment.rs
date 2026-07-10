//! A lean, self-contained snapshot of the news-sentiment verdict at the
//! moment a plan was armed — baked onto [`TradePlan`](crate::trade_plan::TradePlan)
//! for **after-the-fact journalling only**. The worker/engine never reads it.
//!
//! # Why a separate primitive type
//!
//! The rich analysis (`news_sentiment_tv::SentimentAnalysis`) lives in a crate
//! that depends on `forex-factory` (reqwest / scraper / tokio). `core` is the
//! lean, dependency-minimal worker core and must not pull that in. So `tv-arm`
//! (which already has the rich type) converts it down to this flat,
//! string-typed mirror before baking it onto the plan. The stored shape is
//! stable and human-readable, and `core` stays free of the news stack.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The overall news-sentiment verdict captured at arm time, plus a
/// per-currency breakdown. Purely a journalling record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanSentiment {
    /// Start of the lookback window the verdict was computed over.
    pub period_start: DateTime<Utc>,
    /// End of the lookback window — the effective arm instant.
    pub period_end: DateTime<Utc>,
    /// Overall directional verdict for the instrument: `"bullish"`,
    /// `"bearish"`, or `"neutral"`. Stored as the wire string so `core`
    /// needn't share the source enum.
    pub overall_direction: String,
    /// Confidence label: `"high"`, `"medium"`, or `"low"`.
    pub confidence: String,
    /// Per-currency net scores and directions, one entry per currency the
    /// instrument is exposed to that had released events in the window.
    pub currencies: Vec<CurrencySnapshot>,
}

/// One currency's contribution to [`PlanSentiment`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurrencySnapshot {
    /// Currency code, upper-case (e.g. `"EUR"`).
    pub currency: String,
    /// Direction for this currency: `"bullish"` / `"bearish"` / `"neutral"`.
    pub direction: String,
    /// Net score (bullish − bearish weight). Positive = bullish.
    pub net_score: f64,
    /// One human-readable line per released event that moved the score
    /// (e.g. `"Non-Farm Payrolls: Actual (220K) beat forecast (180K)"`).
    pub events: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let snap = PlanSentiment {
            period_start: "2026-05-01T09:30:00Z".parse().unwrap(),
            period_end: "2026-05-02T09:30:00Z".parse().unwrap(),
            overall_direction: "bullish".into(),
            confidence: "high".into(),
            currencies: vec![CurrencySnapshot {
                currency: "EUR".into(),
                direction: "bullish".into(),
                net_score: 3.0,
                events: vec!["GDP q/q: Actual (0.8%) beat forecast (0.5%)".into()],
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: PlanSentiment = serde_json::from_str(&json).unwrap();
        assert_eq!(back, snap);
    }
}
