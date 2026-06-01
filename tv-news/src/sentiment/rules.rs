//! Event-specific sentiment rules.
//!
//! - Most events: higher actual than forecast = bullish for the currency.
//! - Unemployment / claims / deficit: inverted (lower is better).
//! - CPI/PPI/inflation: context-dependent — treated as higher-is-better
//!   here, since central banks tend to tighten, which strengthens the
//!   currency.

use trade_control_cli::{EconomicEvent, Impact};

use super::parser::compare_values;

/// Direction of sentiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SentimentDirection {
    Bullish,
    Bearish,
    #[default]
    Neutral,
}

/// Sentiment analysis result for a single event.
#[derive(Debug, Clone)]
pub struct EventSentiment {
    pub event_name: String,
    pub currency: String,
    pub impact: Impact,
    pub direction: SentimentDirection,
    pub reason: String,
    pub actual: Option<String>,
    pub forecast: Option<String>,
    pub previous: Option<String>,
}

impl EventSentiment {
    /// Weight derived from impact: 3 / 2 / 1.
    pub fn weight(&self) -> f64 {
        match self.impact {
            Impact::High => 3.0,
            Impact::Medium => 2.0,
            Impact::Low => 1.0,
        }
    }
}

/// Rule for how to interpret an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventRule {
    HigherIsBetter,
    LowerIsBetter,
    ContextDependent,
}

/// Pick a rule from the event name (case-insensitive keyword match).
pub fn get_event_rule(event_name: &str) -> EventRule {
    let name_lower = event_name.to_lowercase();
    if name_lower.contains("unemployment")
        || name_lower.contains("jobless")
        || name_lower.contains("claims")
        || name_lower.contains("deficit")
    {
        return EventRule::LowerIsBetter;
    }
    if name_lower.contains("inflation") || name_lower.contains("cpi") || name_lower.contains("ppi")
    {
        return EventRule::ContextDependent;
    }
    EventRule::HigherIsBetter
}

/// Analyze a single economic event and determine its sentiment direction.
pub fn analyze_event(event: &EconomicEvent) -> EventSentiment {
    let rule = get_event_rule(&event.name);

    let (direction, reason) = match (&event.actual, &event.forecast) {
        (Some(actual), Some(forecast)) => match compare_values(actual, forecast) {
            Some(true) => actual_beats_forecast(rule, actual, forecast),
            Some(false) => actual_misses_forecast(rule, actual, forecast),
            None => (
                SentimentDirection::Neutral,
                format!("Actual ({actual}) matched forecast ({forecast})"),
            ),
        },
        (Some(actual), None) => actual_vs_previous(rule, actual, event.previous.as_deref()),
        _ => (
            SentimentDirection::Neutral,
            "No actual data available".to_string(),
        ),
    };

    EventSentiment {
        event_name: event.name.clone(),
        currency: event.currency.clone(),
        impact: event.impact,
        direction,
        reason,
        actual: event.actual.clone(),
        forecast: event.forecast.clone(),
        previous: event.previous.clone(),
    }
}

fn actual_beats_forecast(
    rule: EventRule,
    actual: &str,
    forecast: &str,
) -> (SentimentDirection, String) {
    match rule {
        EventRule::HigherIsBetter => (
            SentimentDirection::Bullish,
            format!("Actual ({actual}) beat forecast ({forecast})"),
        ),
        EventRule::LowerIsBetter => (
            SentimentDirection::Bearish,
            format!("Actual ({actual}) worse than forecast ({forecast}) [lower is better]"),
        ),
        EventRule::ContextDependent => (
            SentimentDirection::Bullish,
            format!("Actual ({actual}) higher than forecast ({forecast}) [context-dependent]"),
        ),
    }
}

fn actual_misses_forecast(
    rule: EventRule,
    actual: &str,
    forecast: &str,
) -> (SentimentDirection, String) {
    match rule {
        EventRule::HigherIsBetter => (
            SentimentDirection::Bearish,
            format!("Actual ({actual}) missed forecast ({forecast})"),
        ),
        EventRule::LowerIsBetter => (
            SentimentDirection::Bullish,
            format!("Actual ({actual}) better than forecast ({forecast}) [lower is better]"),
        ),
        EventRule::ContextDependent => (
            SentimentDirection::Bearish,
            format!("Actual ({actual}) lower than forecast ({forecast}) [context-dependent]"),
        ),
    }
}

fn actual_vs_previous(
    rule: EventRule,
    actual: &str,
    previous: Option<&str>,
) -> (SentimentDirection, String) {
    let Some(previous) = previous else {
        return (
            SentimentDirection::Neutral,
            format!("No forecast or previous to compare with actual ({actual})"),
        );
    };
    match compare_values(actual, previous) {
        Some(true) => match rule {
            EventRule::HigherIsBetter => (
                SentimentDirection::Bullish,
                format!("Actual ({actual}) improved from previous ({previous})"),
            ),
            EventRule::LowerIsBetter => (
                SentimentDirection::Bearish,
                format!("Actual ({actual}) worse than previous ({previous}) [lower is better]"),
            ),
            EventRule::ContextDependent => (
                SentimentDirection::Neutral,
                format!("Actual ({actual}) higher than previous ({previous}) [no forecast]"),
            ),
        },
        Some(false) => match rule {
            EventRule::HigherIsBetter => (
                SentimentDirection::Bearish,
                format!("Actual ({actual}) declined from previous ({previous})"),
            ),
            EventRule::LowerIsBetter => (
                SentimentDirection::Bullish,
                format!("Actual ({actual}) better than previous ({previous}) [lower is better]"),
            ),
            EventRule::ContextDependent => (
                SentimentDirection::Neutral,
                format!("Actual ({actual}) lower than previous ({previous}) [no forecast]"),
            ),
        },
        None => (
            SentimentDirection::Neutral,
            format!("Actual ({actual}) unchanged from previous ({previous})"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn make_event(
        name: &str,
        currency: &str,
        impact: Impact,
        actual: Option<&str>,
        forecast: Option<&str>,
        previous: Option<&str>,
    ) -> EconomicEvent {
        EconomicEvent {
            name: name.to_string(),
            currency: currency.to_string(),
            impact,
            datetime: Local.with_ymd_and_hms(2026, 1, 23, 10, 0, 0).unwrap(),
            actual: actual.map(String::from),
            forecast: forecast.map(String::from),
            previous: previous.map(String::from),
        }
    }

    #[test]
    fn unemployment_rule() {
        assert_eq!(
            get_event_rule("Unemployment Rate"),
            EventRule::LowerIsBetter
        );
        assert_eq!(
            get_event_rule("Initial Jobless Claims"),
            EventRule::LowerIsBetter
        );
    }

    #[test]
    fn default_rule_is_higher_better() {
        assert_eq!(get_event_rule("GDP q/q"), EventRule::HigherIsBetter);
        assert_eq!(
            get_event_rule("Non-Farm Payrolls"),
            EventRule::HigherIsBetter
        );
    }

    #[test]
    fn cpi_is_context_dependent() {
        assert_eq!(get_event_rule("CPI m/m"), EventRule::ContextDependent);
    }

    #[test]
    fn actual_beats_forecast_for_gdp() {
        let event = make_event(
            "GDP q/q",
            "EUR",
            Impact::High,
            Some("0.8%"),
            Some("0.5%"),
            Some("0.4%"),
        );
        let s = analyze_event(&event);
        assert_eq!(s.direction, SentimentDirection::Bullish);
        assert!(s.reason.contains("beat"));
    }

    #[test]
    fn higher_unemployment_is_bearish() {
        let event = make_event(
            "Unemployment Rate",
            "USD",
            Impact::High,
            Some("5.0%"),
            Some("4.5%"),
            Some("4.5%"),
        );
        let s = analyze_event(&event);
        assert_eq!(s.direction, SentimentDirection::Bearish);
    }

    #[test]
    fn lower_unemployment_is_bullish() {
        let event = make_event(
            "Unemployment Rate",
            "USD",
            Impact::High,
            Some("4.0%"),
            Some("4.5%"),
            Some("4.5%"),
        );
        let s = analyze_event(&event);
        assert_eq!(s.direction, SentimentDirection::Bullish);
    }

    #[test]
    fn no_actual_is_neutral() {
        let event = make_event(
            "GDP q/q",
            "EUR",
            Impact::High,
            None,
            Some("0.5%"),
            Some("0.4%"),
        );
        let s = analyze_event(&event);
        assert_eq!(s.direction, SentimentDirection::Neutral);
    }

    #[test]
    fn no_forecast_uses_previous() {
        let event = make_event(
            "GDP q/q",
            "EUR",
            Impact::High,
            Some("0.8%"),
            None,
            Some("0.4%"),
        );
        let s = analyze_event(&event);
        assert_eq!(s.direction, SentimentDirection::Bullish);
        assert!(s.reason.contains("improved"));
    }

    #[test]
    fn weights_by_impact() {
        let high = make_event(
            "Test",
            "EUR",
            Impact::High,
            Some("0.5%"),
            Some("0.5%"),
            None,
        );
        assert!((analyze_event(&high).weight() - 3.0).abs() < 0.001);
        let med = make_event(
            "Test",
            "EUR",
            Impact::Medium,
            Some("0.5%"),
            Some("0.5%"),
            None,
        );
        assert!((analyze_event(&med).weight() - 2.0).abs() < 0.001);
        let low = make_event("Test", "EUR", Impact::Low, Some("0.5%"), Some("0.5%"), None);
        assert!((analyze_event(&low).weight() - 1.0).abs() < 0.001);
    }
}
