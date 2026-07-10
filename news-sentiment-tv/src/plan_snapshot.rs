//! Conversion from the rich [`SentimentAnalysis`] into the lean, string-typed
//! [`PlanSentiment`] that `core` carries on a `TradePlan`.
//!
//! Both consumers that bake sentiment onto a plan — `tv-arm` (arm time) and
//! `replay-candles` (per-window recompute) — go through here so the mapping
//! lives in exactly one place.

use trade_control_core::plan_sentiment::{CurrencySnapshot, PlanSentiment};

use crate::{Confidence, CurrencySentiment, SentimentAnalysis, SentimentDirection};

/// Overall/per-currency direction as its stable wire string.
pub fn direction_str(d: SentimentDirection) -> &'static str {
    match d {
        SentimentDirection::Bullish => "bullish",
        SentimentDirection::Bearish => "bearish",
        SentimentDirection::Neutral => "neutral",
    }
}

/// Confidence label as its stable wire string.
pub fn confidence_str(c: Confidence) -> &'static str {
    match c {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
    }
}

impl SentimentAnalysis {
    /// Convert into the lean journalling snapshot baked onto a `TradePlan`.
    /// Currencies are emitted in sorted order so the read-back is stable
    /// (the underlying map iteration order is not).
    pub fn to_plan_sentiment(&self) -> PlanSentiment {
        let mut currencies: Vec<CurrencySnapshot> =
            self.currency_sentiments.values().map(snapshot).collect();
        currencies.sort_by(|x, y| x.currency.cmp(&y.currency));

        PlanSentiment {
            period_start: self.period_start.to_utc(),
            period_end: self.period_end.to_utc(),
            overall_direction: direction_str(self.overall_direction).to_string(),
            confidence: confidence_str(self.confidence).to_string(),
            currencies,
        }
    }
}

fn snapshot(cs: &CurrencySentiment) -> CurrencySnapshot {
    CurrencySnapshot {
        currency: cs.currency.clone(),
        direction: direction_str(cs.direction).to_string(),
        net_score: cs.net_score(),
        events: cs
            .events
            .iter()
            .map(|ev| format!("{}: {}", ev.event_name, ev.reason))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze_sentiment;
    use chrono::{Local, TimeZone};
    use forex_factory::{EconomicEvent, Impact};

    fn event(currency: &str, actual: &str, forecast: &str) -> EconomicEvent {
        EconomicEvent {
            name: format!("{currency} GDP q/q"),
            currency: currency.to_string(),
            impact: Impact::High,
            datetime: Local.with_ymd_and_hms(2026, 1, 23, 10, 0, 0).unwrap(),
            actual: Some(actual.to_string()),
            forecast: Some(forecast.to_string()),
            previous: None,
        }
    }

    #[test]
    fn to_plan_sentiment_mirrors_and_sorts() {
        let at = Local.with_ymd_and_hms(2026, 1, 23, 14, 0, 0).unwrap();
        let events = vec![event("USD", "150K", "180K"), event("EUR", "0.8%", "0.5%")];
        let ccys = vec!["EUR".to_string(), "USD".to_string()];
        let a = analyze_sentiment(&ccys, &events, at);

        let snap = a.to_plan_sentiment();
        assert_eq!(snap.overall_direction, direction_str(a.overall_direction));
        assert_eq!(snap.confidence, confidence_str(a.confidence));
        assert_eq!(snap.currencies.len(), a.currency_sentiments.len());
        // Sorted by currency for a stable read-back.
        assert!(
            snap.currencies
                .windows(2)
                .all(|w| w[0].currency <= w[1].currency)
        );
        let eur = snap
            .currencies
            .iter()
            .find(|c| c.currency == "EUR")
            .expect("EUR present");
        assert_eq!(eur.direction, "bullish");
        assert!(eur.events.iter().any(|e| e.contains("GDP q/q")));
    }
}
