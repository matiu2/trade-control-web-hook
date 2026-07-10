//! News-sentiment analysis for the tv-news / tv-arm / replay-candles tools.
//!
//! Aggregates released forex-factory events from the recent past into a
//! per-currency and overall directional verdict for an instrument's
//! `news_currencies`. Extracted from the `tv-news` binary so `tv-arm`
//! (arm-time snapshot) and `replay-candles` (per-window recompute) can
//! reuse the exact same algorithm, and so the result can be serialized
//! into a `TradePlan` for after-the-fact journalling.
//!
//! The output types derive `serde::{Serialize, Deserialize}` — the only
//! change from the original `tv-news` copy; the algorithm is identical.

mod plan_snapshot;
mod sentiment;

pub use plan_snapshot::{confidence_str, direction_str};
pub use sentiment::{
    Confidence, CurrencySentiment, EventSentiment, SentimentAnalysis, SentimentDirection,
    analyze_event, analyze_sentiment, sentiment_lookback_start,
};
