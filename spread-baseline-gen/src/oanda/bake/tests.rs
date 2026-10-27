use super::*;
use chrono::{TimeZone, Utc};

fn fixture() -> Report {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let from = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let mut profile = crate::compute::SpreadProfile::empty(90_000);
    profile.review = ReviewStatus::Reviewed;
    profile.baseline_median_pips = 0.8;
    profile.baseline_low_pips = 0.7;
    profile.baseline_high_pips = 1.0;
    Report {
        account: "oanda-live".into(),
        account_id: "001-011-00000000-001".into(),
        source: "oanda-live-account-candles-MBA".into(),
        cache_namespace: oanda_client::account_candle_namespace("001-011-00000000-001"),
        from_utc: from,
        to_utc: from + chrono::Duration::days(90),
        requested_symbols: vec!["EUR_USD".into()],
        catalogue_symbols: vec!["EUR_USD".into()],
        instruments: vec![super::super::Measurement {
            symbol: "EUR_USD".into(),
            schedule: "ny".into(),
            pip_size: 0.0001,
            first_utc: from,
            last_utc: from + chrono::Duration::days(89),
            observed_utc_days: 64,
            profile,
        }],
        failures: vec![],
        complete: true,
    }
}

#[test]
fn live_bake_never_replaces_the_unqualified_demo_profile() {
    let text = render(&fixture()).unwrap();
    assert!(text.contains("SPREAD_BASELINE_OANDA"));
    assert!(text.contains("\"oanda:oanda-live:EUR_USD\""));
    assert!(!text.contains("\"oanda\", \"EUR_USD\""));
    assert!(
        !text.contains(&fixture().account_id),
        "Public bake must omit broker account numbers"
    );
}

#[test]
fn incomplete_duplicate_short_and_unreviewed_evidence_is_refused() {
    let base = fixture();
    let mut report = base.clone();
    report.complete = false;
    assert!(render(&report).is_err());
    report = base.clone();
    report.requested_symbols.push("GBP_USD".into());
    assert!(render(&report).is_err());
    report = base.clone();
    report.instruments.push(report.instruments[0].clone());
    assert!(render(&report).is_err());
    report = base.clone();
    report.to_utc = report.from_utc + chrono::Duration::days(89);
    assert!(render(&report).is_err());
    report = base;
    report.instruments[0].profile.review = ReviewStatus::InsufficientData;
    assert!(render(&report).is_err());
}

#[test]
fn a_reviewed_few_day_sample_or_sparse_dates_cannot_claim_ninety_days() {
    let mut report = fixture();
    report.instruments[0].first_utc = report.to_utc - chrono::Duration::days(3);
    assert!(render(&report).is_err());
    report = fixture();
    report.instruments[0].last_utc = report.from_utc + chrono::Duration::days(3);
    assert!(render(&report).is_err());
    report = fixture();
    report.instruments[0].observed_utc_days = 3;
    assert!(render(&report).is_err());
}
