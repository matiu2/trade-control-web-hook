use super::*;
use crate::{SpreadProfile, compute::ReviewStatus};

fn report() -> Report {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .ok();
    let from = "2026-07-01T00:00:00Z".parse().unwrap();
    let to = "2026-10-01T00:00:00Z".parse().unwrap();
    let mut profile = SpreadProfile::empty(1000);
    profile.review = ReviewStatus::Reviewed;
    profile.baseline_low_pips = 0.1;
    profile.baseline_median_pips = 0.2;
    profile.baseline_high_pips = 0.5;
    profile.hour_p90_frac = [0.00002; 24];
    Report {
        schema_version: 1,
        identity: super::super::Identity {
            profile: "demo".into(),
            environment: "demo".into(),
            endpoint: "wss://demo.ctraderapi.com:5036".into(),
            account_id: 42,
            broker: "Example".into(),
            login: 123,
        },
        source: "paired-ticks-m1".into(),
        from_utc: from,
        to_utc: to,
        requested_symbols: vec!["EURUSD".into()],
        complete: true,
        excluded: vec![],
        failures: vec![],
        instruments: vec![super::super::InstrumentReport {
            symbol_id: 1,
            measurement: crate::mt5::InstrumentReport {
                symbol: "EURUSD".into(),
                pip_size: 0.0001,
                schedule: "ny".into(),
                timezone: "America/New_York".into(),
                first_minute_utc: from,
                last_minute_utc: to - chrono::Duration::minutes(1),
                elevated_local_hours: vec![],
                profile,
            },
        }],
    }
}

#[test]
fn paired_account_scoped_table_retains_forecast() {
    crate::ctrader::test_logging();
    let r = report();
    let output = render(&r, &r.identity).unwrap();
    assert!(output.contains("SPREAD_BASELINE_CTRADER"));
    assert!(output.contains("\"ctrader-demo\", \"ctrader:demo:EURUSD\", \"ny\", true"));
    assert!(output.contains("2e-5"));
}

#[test]
fn wrong_identity_native_quotes_thin_window_and_changed_metadata_fail() {
    crate::ctrader::test_logging();
    let r = report();
    let mut identity = r.identity.clone();
    identity.account_id += 1;
    assert!(render(&r, &identity).is_err());
    for field in [
        "source",
        "window",
        "pip",
        "schedule",
        "review",
        "incomplete",
        "live",
    ] {
        let mut r = report();
        match field {
            "source" => r.source = "native-bid".into(),
            "window" => r.from_utc = r.to_utc - chrono::Duration::days(89),
            "pip" => r.instruments[0].measurement.pip_size = 0.001,
            "schedule" => r.instruments[0].measurement.timezone = "UTC".into(),
            "review" => {
                r.instruments[0].measurement.profile.review = ReviewStatus::InsufficientData
            }
            "incomplete" => r.complete = false,
            "live" => r.identity.environment = "live".into(),
            _ => unreachable!(),
        }
        assert!(render(&r, &r.identity).is_err(), "accepted {field}");
    }
}

#[test]
fn failures_and_unattempted_symbols_cannot_be_silently_baked() {
    crate::ctrader::test_logging();
    let mut r = report();
    r.requested_symbols.push("AUDUSD".into());
    assert!(render(&r, &r.identity).is_err());
    r.failures.push(crate::mt5::Failure {
        symbol: "AUDUSD".into(),
        error: "tick history missing".into(),
    });
    assert!(render(&r, &r.identity).is_err());
}

#[test]
fn partial_bake_requires_all_symbols_attempted_and_preserves_account_scope() {
    crate::ctrader::test_logging();
    let mut r = report();
    r.requested_symbols.push("AUDUSD".into());
    r.complete = false;
    assert!(render_finished(&r, &r.identity).is_err());
    r.failures.push(crate::mt5::Failure {
        symbol: "AUDUSD".into(),
        error: "broker history unavailable".into(),
    });
    let table = render_finished(&r, &r.identity).unwrap();
    assert!(table.contains("ctrader:demo:EURUSD"));
    assert!(!table.contains("AUDUSD"));
    assert_eq!(r.failures.len(), 1);
    r.instruments[0].measurement.profile.review = ReviewStatus::InsufficientData;
    assert!(render_finished(&r, &r.identity).is_err());
}

#[test]
fn invalid_forecast_percentiles_and_duplicate_symbols_are_rejected() {
    crate::ctrader::test_logging();
    for field in [
        "forecast",
        "widen",
        "percentile",
        "duplicate",
        "symbol_id",
        "bounds",
    ] {
        let mut r = report();
        match field {
            "forecast" => r.instruments[0].measurement.profile.hour_p90_frac[3] = f64::NAN,
            "widen" => r.instruments[0].measurement.profile.hour_widen_frac[3] = 0.001,
            "percentile" => r.instruments[0].measurement.profile.baseline_low_pips = 1.0,
            "duplicate" => r.instruments.push(r.instruments[0].clone()),
            "symbol_id" => r.instruments[0].symbol_id = 0,
            "bounds" => r.instruments[0].measurement.last_minute_utc = r.to_utc,
            _ => unreachable!(),
        }
        assert!(render(&r, &r.identity).is_err(), "accepted {field}");
    }
}
