//! Sentiment analysis — aggregate released forex-factory events from
//! the recent past into a per-currency and overall direction for the
//! chart's instrument.
//!
//! Ported from `trade-calendar-maker::sentiment` and adapted to consume
//! a `news_currencies: &[String]` slice (matching what
//! `instrument-lookup::Asset` carries) rather than a project-local
//! `Instrument` type. Algorithm is otherwise unchanged.
//!
//! Lookback window: 24 hours, except on Mondays where we reach back to
//! Friday so weekend news isn't dropped. Only events with an `actual`
//! value are considered (un-released items don't move the score).

mod parser;
mod rules;

pub use rules::{EventSentiment, SentimentDirection, analyze_event};

use chrono::{DateTime, Datelike, Local, TimeDelta, Weekday};
use forex_factory::{EconomicEvent, Impact};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Start of the sentiment lookback window relative to `at`.
///
/// - Monday: 3 days back (covers Friday + weekend).
/// - Any other weekday: 24 hours back.
pub fn sentiment_lookback_start(at: DateTime<Local>) -> DateTime<Local> {
    if at.weekday() == Weekday::Mon {
        at - TimeDelta::days(3)
    } else {
        at - TimeDelta::hours(24)
    }
}

/// Overall sentiment analysis result for an instrument.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentimentAnalysis {
    pub period_start: DateTime<Local>,
    pub period_end: DateTime<Local>,
    pub currency_sentiments: HashMap<String, CurrencySentiment>,
    pub overall_direction: SentimentDirection,
    pub confidence: Confidence,
}

/// Sentiment scored for a single currency.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CurrencySentiment {
    pub currency: String,
    pub events: Vec<EventSentiment>,
    pub bullish_score: f64,
    pub bearish_score: f64,
    pub direction: SentimentDirection,
}

impl CurrencySentiment {
    fn new(currency: String) -> Self {
        Self {
            currency,
            events: Vec::new(),
            bullish_score: 0.0,
            bearish_score: 0.0,
            direction: SentimentDirection::Neutral,
        }
    }

    fn add_event(&mut self, event: EventSentiment) {
        let weight = event.weight();
        match event.direction {
            SentimentDirection::Bullish => self.bullish_score += weight,
            SentimentDirection::Bearish => self.bearish_score += weight,
            SentimentDirection::Neutral => {}
        }
        self.events.push(event);
    }

    fn finalize(&mut self) {
        let net = self.bullish_score - self.bearish_score;
        self.direction = if net > 0.5 {
            SentimentDirection::Bullish
        } else if net < -0.5 {
            SentimentDirection::Bearish
        } else {
            SentimentDirection::Neutral
        };
    }

    /// Net score (bullish − bearish). Positive = bullish.
    pub fn net_score(&self) -> f64 {
        self.bullish_score - self.bearish_score
    }
}

/// Confidence label for the overall analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

/// Run the sentiment pipeline.
///
/// - `news_currencies`: the instrument's currencies in chart order
///   (e.g. `["EUR", "USD"]` for EUR/USD). For FX pairs the first is
///   base, second is quote; bullish quote inverts the pair direction.
///   For indices/commodities, the first entry is the primary currency.
/// - `events`: the raw event list; this function filters to the
///   lookback window, released events only, and currencies the
///   instrument is exposed to.
/// - `at`: "now" — drives the lookback start and the period_end label.
pub fn analyze_sentiment(
    news_currencies: &[String],
    events: &[EconomicEvent],
    at: DateTime<Local>,
) -> SentimentAnalysis {
    let period_start = sentiment_lookback_start(at);
    let period_end = at;

    let in_scope = |c: &str| news_currencies.iter().any(|x| x.eq_ignore_ascii_case(c));

    let relevant_events: Vec<&EconomicEvent> = events
        .iter()
        .filter(|e| {
            e.datetime >= period_start
                && e.datetime <= period_end
                && e.actual.is_some()
                && in_scope(&e.currency)
        })
        .collect();

    let mut currency_sentiments: HashMap<String, CurrencySentiment> = HashMap::new();
    for event in &relevant_events {
        let sentiment = analyze_event(event);
        let key = event.currency.to_uppercase();
        let entry = currency_sentiments
            .entry(key.clone())
            .or_insert_with(|| CurrencySentiment::new(key));
        entry.add_event(sentiment);
    }
    for cs in currency_sentiments.values_mut() {
        cs.finalize();
    }

    let overall_direction = calculate_overall_direction(news_currencies, &currency_sentiments);
    let confidence = calculate_confidence(&relevant_events, &currency_sentiments);

    SentimentAnalysis {
        period_start,
        period_end,
        currency_sentiments,
        overall_direction,
        confidence,
    }
}

/// Combine per-currency directions into a single direction for the
/// instrument. For 2-currency FX pairs the quote is inverted; for
/// single-currency instruments (indices, gold/USD, etc.) the primary
/// currency wins.
fn calculate_overall_direction(
    news_currencies: &[String],
    currency_sentiments: &HashMap<String, CurrencySentiment>,
) -> SentimentDirection {
    if news_currencies.len() == 2 {
        let base = news_currencies[0].to_uppercase();
        let quote = news_currencies[1].to_uppercase();
        let base_dir = currency_sentiments
            .get(&base)
            .map(|cs| cs.direction)
            .unwrap_or_default();
        let quote_dir = currency_sentiments
            .get(&quote)
            .map(|cs| cs.direction)
            .unwrap_or_default();
        let base_effect = direction_to_effect(base_dir);
        let quote_effect = -direction_to_effect(quote_dir);
        let net = base_effect + quote_effect;
        if net > 0.0 {
            SentimentDirection::Bullish
        } else if net < 0.0 {
            SentimentDirection::Bearish
        } else {
            SentimentDirection::Neutral
        }
    } else if let Some(primary) = news_currencies.first() {
        currency_sentiments
            .get(&primary.to_uppercase())
            .map(|cs| cs.direction)
            .unwrap_or_default()
    } else {
        SentimentDirection::Neutral
    }
}

fn direction_to_effect(d: SentimentDirection) -> f64 {
    match d {
        SentimentDirection::Bullish => 1.0,
        SentimentDirection::Bearish => -1.0,
        SentimentDirection::Neutral => 0.0,
    }
}

fn calculate_confidence(
    events: &[&EconomicEvent],
    currency_sentiments: &HashMap<String, CurrencySentiment>,
) -> Confidence {
    let high_impact_count = events.iter().filter(|e| e.impact == Impact::High).count();
    let total_events = events.len();

    let directions: Vec<_> = currency_sentiments
        .values()
        .map(|cs| cs.direction)
        .filter(|d| *d != SentimentDirection::Neutral)
        .collect();
    let all_same_direction = directions.windows(2).all(|w| w[0] == w[1]);

    if high_impact_count >= 2 && total_events >= 3 && all_same_direction {
        Confidence::High
    } else if high_impact_count >= 1 || total_events >= 2 {
        Confidence::Medium
    } else {
        Confidence::Low
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn event(
        currency: &str,
        impact: Impact,
        actual: Option<&str>,
        forecast: Option<&str>,
        previous: Option<&str>,
        when: DateTime<Local>,
    ) -> EconomicEvent {
        EconomicEvent {
            name: format!("{currency} Test Event"),
            currency: currency.to_string(),
            impact,
            datetime: when,
            actual: actual.map(String::from),
            forecast: forecast.map(String::from),
            previous: previous.map(String::from),
        }
    }

    fn ccys(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn filters_unreleased_events() {
        let at = Local.with_ymd_and_hms(2026, 1, 23, 14, 0, 0).unwrap();
        let when = Local.with_ymd_and_hms(2026, 1, 23, 10, 0, 0).unwrap();
        let events = vec![event("EUR", Impact::High, None, Some("0.5%"), None, when)];
        let a = analyze_sentiment(&ccys(&["EUR", "USD"]), &events, at);
        assert!(a.currency_sentiments.is_empty());
    }

    #[test]
    fn bullish_base_currency_is_bullish_pair() {
        let at = Local.with_ymd_and_hms(2026, 1, 23, 14, 0, 0).unwrap();
        let when = Local.with_ymd_and_hms(2026, 1, 23, 10, 0, 0).unwrap();
        let events = vec![event(
            "EUR",
            Impact::High,
            Some("0.8%"),
            Some("0.5%"),
            None,
            when,
        )];
        let a = analyze_sentiment(&ccys(&["EUR", "USD"]), &events, at);
        assert_eq!(a.overall_direction, SentimentDirection::Bullish);
    }

    #[test]
    fn bullish_quote_currency_is_bearish_pair() {
        let at = Local.with_ymd_and_hms(2026, 1, 23, 14, 0, 0).unwrap();
        let when = Local.with_ymd_and_hms(2026, 1, 23, 10, 0, 0).unwrap();
        let events = vec![event(
            "USD",
            Impact::High,
            Some("220K"),
            Some("180K"),
            None,
            when,
        )];
        let a = analyze_sentiment(&ccys(&["EUR", "USD"]), &events, at);
        assert_eq!(a.overall_direction, SentimentDirection::Bearish);
    }

    #[test]
    fn drops_events_outside_lookback_window() {
        let at = Local.with_ymd_and_hms(2026, 1, 23, 14, 0, 0).unwrap();
        let two_days_ago = Local.with_ymd_and_hms(2026, 1, 21, 10, 0, 0).unwrap();
        let events = vec![event(
            "EUR",
            Impact::High,
            Some("0.8%"),
            Some("0.5%"),
            None,
            two_days_ago,
        )];
        let a = analyze_sentiment(&ccys(&["EUR", "USD"]), &events, at);
        assert!(a.currency_sentiments.is_empty());
    }

    #[test]
    fn drops_events_for_unrelated_currency() {
        let at = Local.with_ymd_and_hms(2026, 1, 23, 14, 0, 0).unwrap();
        let when = Local.with_ymd_and_hms(2026, 1, 23, 10, 0, 0).unwrap();
        let events = vec![event(
            "GBP",
            Impact::High,
            Some("0.8%"),
            Some("0.5%"),
            None,
            when,
        )];
        let a = analyze_sentiment(&ccys(&["EUR", "USD"]), &events, at);
        assert!(a.currency_sentiments.is_empty());
    }

    #[test]
    fn monday_lookback_includes_friday_events() {
        let monday = Local.with_ymd_and_hms(2026, 1, 26, 14, 0, 0).unwrap();
        assert_eq!(monday.weekday(), Weekday::Mon);
        let friday = Local.with_ymd_and_hms(2026, 1, 23, 16, 0, 0).unwrap();
        let events = vec![event(
            "EUR",
            Impact::High,
            Some("0.8%"),
            Some("0.5%"),
            None,
            friday,
        )];
        let a = analyze_sentiment(&ccys(&["EUR", "USD"]), &events, monday);
        assert_eq!(a.overall_direction, SentimentDirection::Bullish);
    }

    #[test]
    fn tuesday_lookback_excludes_friday_events() {
        let tuesday = Local.with_ymd_and_hms(2026, 1, 27, 14, 0, 0).unwrap();
        let friday = Local.with_ymd_and_hms(2026, 1, 23, 10, 0, 0).unwrap();
        let events = vec![event(
            "EUR",
            Impact::High,
            Some("0.8%"),
            Some("0.5%"),
            None,
            friday,
        )];
        let a = analyze_sentiment(&ccys(&["EUR", "USD"]), &events, tuesday);
        assert!(a.currency_sentiments.is_empty());
    }

    #[test]
    fn high_confidence_when_multiple_high_impact_aligned() {
        let at = Local.with_ymd_and_hms(2026, 1, 23, 14, 0, 0).unwrap();
        let w = Local.with_ymd_and_hms(2026, 1, 23, 8, 0, 0).unwrap();
        let events = vec![
            event("EUR", Impact::High, Some("0.8%"), Some("0.5%"), None, w),
            event("EUR", Impact::High, Some("0.9%"), Some("0.6%"), None, w),
            event("EUR", Impact::Medium, Some("0.7%"), Some("0.5%"), None, w),
        ];
        let a = analyze_sentiment(&ccys(&["EUR", "USD"]), &events, at);
        assert_eq!(a.confidence, Confidence::High);
    }

    #[test]
    fn single_currency_instrument_uses_primary() {
        // Index-style: only one currency in the catalog entry.
        let at = Local.with_ymd_and_hms(2026, 1, 23, 14, 0, 0).unwrap();
        let when = Local.with_ymd_and_hms(2026, 1, 23, 10, 0, 0).unwrap();
        let events = vec![event(
            "USD",
            Impact::High,
            Some("220K"),
            Some("180K"),
            None,
            when,
        )];
        let a = analyze_sentiment(&ccys(&["USD"]), &events, at);
        // For an index in USD, bullish USD is bullish for the instrument.
        assert_eq!(a.overall_direction, SentimentDirection::Bullish);
    }

    #[test]
    fn case_insensitive_currency_filter() {
        let at = Local.with_ymd_and_hms(2026, 1, 23, 14, 0, 0).unwrap();
        let when = Local.with_ymd_and_hms(2026, 1, 23, 10, 0, 0).unwrap();
        // Event's currency is lower-case.
        let events = vec![event(
            "eur",
            Impact::High,
            Some("0.8%"),
            Some("0.5%"),
            None,
            when,
        )];
        let a = analyze_sentiment(&ccys(&["EUR", "USD"]), &events, at);
        assert_eq!(a.overall_direction, SentimentDirection::Bullish);
    }

    #[test]
    fn serde_round_trip_preserves_analysis() {
        let at = Local.with_ymd_and_hms(2026, 1, 23, 14, 0, 0).unwrap();
        let when = Local.with_ymd_and_hms(2026, 1, 23, 10, 0, 0).unwrap();
        let events = vec![
            event("EUR", Impact::High, Some("0.8%"), Some("0.5%"), None, when),
            event(
                "USD",
                Impact::Medium,
                Some("150K"),
                Some("180K"),
                None,
                when,
            ),
        ];
        let a = analyze_sentiment(&ccys(&["EUR", "USD"]), &events, at);
        let json = serde_json::to_string(&a).unwrap();
        let back: SentimentAnalysis = serde_json::from_str(&json).unwrap();
        assert_eq!(back.overall_direction, a.overall_direction);
        assert_eq!(back.confidence, a.confidence);
        assert_eq!(back.currency_sentiments.len(), a.currency_sentiments.len());
        for (k, v) in &a.currency_sentiments {
            let bv = back.currency_sentiments.get(k).unwrap();
            assert_eq!(bv.direction, v.direction);
            assert!((bv.net_score() - v.net_score()).abs() < 1e-9);
            assert_eq!(bv.events.len(), v.events.len());
        }
    }
}
