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
            "pip" => r.instruments[0].measurement.pip_size = 0.0,
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

#[test]
fn broker_native_pip_conventions_calibrate_and_bake_without_rescaling_prices() {
    crate::ctrader::test_logging();
    for (symbol, native_pip, price, spread) in [
        ("EURCZK", 0.001, 25.0, 0.01),
        ("EURHUF", 0.01, 390.0, 0.02),
        ("GBPZAR", 0.0001, 22.0, 0.001),
    ] {
        let mut r = report();
        let asset = crate::ctrader::asset(symbol).unwrap();
        assert_ne!(native_pip, asset.pip_size);
        let candles = (0..1440)
            .map(|minute| {
                let mid = price + (minute / 60) as f64 * 0.001;
                let bid = mid - spread / 2.0;
                let ask = mid + spread / 2.0;
                candle_model::BidAskCandleData {
                    timestamp: (r.from_utc + chrono::Duration::minutes(minute)).fixed_offset(),
                    open: mid,
                    high: mid,
                    low: mid,
                    close: mid,
                    volume: 1.0,
                    bid_open: bid,
                    bid_high: bid,
                    bid_low: bid,
                    bid_close: bid,
                    ask_open: ask,
                    ask_high: ask,
                    ask_low: ask,
                    ask_close: ask,
                }
            })
            .collect::<Vec<_>>();
        let timezone = asset.spread_schedule_tz().unwrap().parse().unwrap();
        let measured = crate::mt5::instrument_report(
            symbol,
            native_pip,
            &asset.spread_schedule,
            timezone,
            &candles,
        )
        .unwrap();
        let canonical = crate::mt5::instrument_report(
            symbol,
            asset.pip_size,
            &asset.spread_schedule,
            timezone,
            &candles,
        )
        .unwrap();
        assert_eq!(
            measured.profile.hour_p90_frac,
            canonical.profile.hour_p90_frac
        );
        assert_eq!(
            measured.profile.hour_widen_frac,
            canonical.profile.hour_widen_frac
        );
        assert_eq!(
            measured.profile.elevated_hours,
            canonical.profile.elevated_hours
        );
        assert!((measured.profile.baseline_median_pips - spread / native_pip).abs() < 1e-8);
        assert_eq!(measured.pip_size, native_pip);
        r.requested_symbols = vec![symbol.into()];
        r.instruments[0].measurement = measured;
        assert!(
            render(&r, &r.identity)
                .unwrap()
                .contains(&format!("ctrader:demo:{symbol}"))
        );
    }
}

#[test]
fn fresh_broker_metadata_must_match_the_native_units_recorded_in_each_row() {
    crate::ctrader::test_logging();
    for (symbol, pip) in [("EURCZK", 0.001), ("EURHUF", 0.01), ("GBPZAR", 0.0001)] {
        let mut r = report();
        let row = &mut r.instruments[0];
        row.measurement.symbol = symbol.into();
        row.measurement.pip_size = pip;
        verify_broker_metadata(row, row.symbol_id, symbol, pip).unwrap();
        assert!(verify_broker_metadata(row, row.symbol_id, symbol, pip * 10.0).is_err());
        assert!(verify_broker_metadata(row, row.symbol_id + 1, symbol, pip).is_err());
        assert!(verify_broker_metadata(row, row.symbol_id, "WRONG", pip).is_err());
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            row.measurement.pip_size = invalid;
            assert!(verify_broker_metadata(row, row.symbol_id, symbol, invalid).is_err());
        }
    }
}

#[test]
fn invalid_broker_native_units_and_non_fx_reports_still_cannot_be_baked() {
    crate::ctrader::test_logging();
    for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let mut r = report();
        r.instruments[0].measurement.pip_size = invalid;
        assert!(render(&r, &r.identity).is_err());
    }
    let mut r = report();
    r.requested_symbols = vec!["XAUUSD".into()];
    r.instruments[0].measurement.symbol = "XAUUSD".into();
    assert!(render(&r, &r.identity).is_err());
}
