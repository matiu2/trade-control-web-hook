use super::*;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

fn candle(minute: i64, bid: f64, ask: f64) -> BidAskCandleData {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .ok();
    BidAskCandleData {
        timestamp: DateTime::from_timestamp(1_791_399_600 + minute * 60, 0)
            .unwrap().fixed_offset(),
        open: 1.0, high: 1.0, low: 1.0, close: (bid + ask) / 2.0,
        bid_open: bid, bid_high: bid, bid_low: bid, bid_close: bid,
        ask_open: ask, ask_high: ask, ask_low: ask, ask_close: ask,
        volume: 0.0,
    }
}

#[test]
fn invalid_quotes_are_errors_rather_than_dropped_samples() {
    let result = instrument_report("EURCAD", 0.0001, "ny", chrono_tz::America::New_York,
        &[candle(0, 1.1, 1.0)]);
    assert!(result.is_err());
}

#[test]
fn duplicate_minutes_cannot_bias_the_profile() {
    let c = candle(0, 1.0, 1.0002);
    assert!(instrument_report("EURCAD", 0.0001, "ny", chrono_tz::America::New_York,
        &[c.clone(), c]).is_err());
}

#[test]
fn thin_samples_are_explicitly_unreviewed() {
    let r = instrument_report("EURCAD", 0.0001, "ny", chrono_tz::America::New_York,
        &[candle(0, 1.0, 1.0002)]).unwrap();
    let json = serde_json::to_value(r).unwrap();
    assert_eq!(json["profile"]["review"], "InsufficientData");
    assert_eq!(json["timezone"], "America/New_York");
    assert_eq!(json["profile"]["n_bars"], 1);
}
