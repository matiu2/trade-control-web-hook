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
        quarantine_policy: None,
        quarantine_audit: None,
        excluded: vec![],
        failures: vec![],
        instruments: vec![super::super::InstrumentReport {
            symbol_id: 1,
            quarantined_minutes: Vec::new(),
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
fn invalid_broker_native_units_and_unknown_symbols_still_cannot_be_baked() {
    crate::ctrader::test_logging();
    for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let mut r = report();
        r.instruments[0].measurement.pip_size = invalid;
        assert!(render(&r, &r.identity).is_err());
    }
    let mut r = report();
    r.requested_symbols = vec!["NO_SUCH_SYMBOL".into()];
    r.instruments[0].measurement.symbol = "NO_SUCH_SYMBOL".into();
    assert!(render(&r, &r.identity).is_err());
}

#[test]
fn explicit_palladium_report_uses_native_quotes_without_enabling_other_commodities() {
    crate::ctrader::test_logging();
    let mut r = report();
    r.requested_symbols = vec!["XPDUSD".into()];
    let row = &mut r.instruments[0];
    row.symbol_id = 22346;
    row.measurement.symbol = "XPDUSD".into();
    row.measurement.pip_size = 0.01;
    let table = render(&r, &r.identity).unwrap();
    assert!(table.contains("ctrader:demo:XPDUSD"));
    verify_broker_metadata(&r.instruments[0], 22346, "XPDUSD", 0.01).unwrap();
    assert!(verify_broker_metadata(&r.instruments[0], 22346, "XPDUSD", 0.001).is_err());
    assert!(verify_broker_metadata(&r.instruments[0], 22346, "XPD_USD", 0.01).is_err());
    r.instruments[0].measurement.profile.review = ReviewStatus::InsufficientData;
    assert!(render(&r, &r.identity).is_err());
    r.instruments[0].measurement.profile.review = ReviewStatus::Reviewed;
    r.complete = false;
    assert!(render(&r, &r.identity).is_err());
}

#[test]
fn gold_cross_uses_native_identity_without_a_class_specific_exemption() {
    crate::ctrader::test_logging();
    let mut r = report();
    r.requested_symbols = vec!["XAUEUR".into()];
    let row = &mut r.instruments[0];
    row.symbol_id = 22312;
    row.measurement.symbol = "XAUEUR".into();
    row.measurement.pip_size = 0.01;
    let table = render(&r, &r.identity).unwrap();
    assert!(table.contains("ctrader:demo:XAUEUR"));
    verify_broker_metadata(&r.instruments[0], 22312, "XAUEUR", 0.01).unwrap();
    assert!(verify_broker_metadata(&r.instruments[0], 22312, "XAU_EUR", 0.01).is_err());
    assert!(verify_broker_metadata(&r.instruments[0], 22312, "XAUEUR", 0.1).is_err());
}

#[test]
fn stock_bake_retains_all_exact_native_names_and_short_session_quality_rules() {
    crate::ctrader::test_logging();
    let mut r = report();
    r.identity.profile = "ctrader-demo".into();
    r.identity.account_id = 49247794;
    r.identity.broker = "Spotware".into();
    r.identity.login = 5944473;
    let template = r.instruments[0].clone();
    r.instruments = broker_ctrader::catalogue::stock_symbols()
        .unwrap()
        .into_iter()
        .map(|symbol| {
            let native = broker_ctrader::catalogue::stock_metadata(symbol).unwrap();
            let mut row = template.clone();
            row.symbol_id = native.id;
            row.measurement.symbol = symbol.into();
            row.measurement.pip_size = 10_f64.powi(-native.pip_position);
            row.measurement.schedule = "utc".into();
            row.measurement.timezone = "UTC".into();
            row.measurement.profile.review = ReviewStatus::MaskNotComputable;
            row
        })
        .collect();
    r.requested_symbols = r
        .instruments
        .iter()
        .map(|row| row.measurement.symbol.clone())
        .collect();
    let output = render(&r, &r.identity).unwrap();
    let info = super::super::output::table_info(&output).unwrap();
    assert_eq!(info.keys.len(), 350);
    for symbol in &r.requested_symbols {
        assert!(
            info.keys
                .contains(&format!("ctrader:ctrader-demo:{symbol}"))
        );
    }
    r.instruments[0].measurement.profile.n_bars = MIN_INSTRUMENT_BARS - 1;
    assert!(render(&r, &r.identity).is_err());
}

#[test]
fn qualified_complete_profile_requires_durable_matching_bounded_exclusions() {
    use crate::ctrader::quarantine::{Binding, Minute, Policy, audit::Audit};
    crate::ctrader::test_logging();
    let dir = tempfile::tempdir().unwrap();
    let mut r = report();
    r.quarantine_policy = Some(Policy::new(5).unwrap());
    let mut audit = Audit::open(
        &dir.path().join("audit.json"),
        None,
        Binding::from_report(&r).unwrap(),
    )
    .unwrap();
    let crossed = ctrader_data_source::CrossedHistoricalQuote {
        time_ms: (r.from_utc + chrono::Duration::minutes(1)).timestamp_millis() + 123,
        bid: 2.0,
        ask: 1.0,
    };
    let minute = Minute::from_error("EURUSD", 1, &crossed).unwrap();
    audit.record(minute.clone()).unwrap();
    r.instruments[0].quarantined_minutes.push(minute);
    r.quarantine_audit = Some(audit.evidence().unwrap());
    let output = render(&r, &r.identity).unwrap();
    assert!(output.contains("calibration excludes 1 crossed M1 minutes, limit 5"));
    crate::ctrader::output::table_info(&output).unwrap();
    crate::ctrader::output::verify_present(&output, &output).unwrap();
    for field in [
        "digest",
        "missing-row-evidence",
        "duplicate",
        "budget",
        "window",
        "policy",
    ] {
        let mut invalid = r.clone();
        match field {
            "digest" => invalid.quarantine_audit.as_mut().unwrap().sha256 = "0".repeat(64),
            "missing-row-evidence" => invalid.instruments[0].quarantined_minutes.clear(),
            "duplicate" => {
                let minute = invalid.instruments[0].quarantined_minutes[0].clone();
                invalid.instruments[0].quarantined_minutes.push(minute);
            }
            "budget" => {
                invalid
                    .quarantine_policy
                    .as_mut()
                    .unwrap()
                    .max_minutes_per_instrument = 6
            }
            "window" => invalid.from_utc += chrono::Duration::minutes(1),
            "policy" => invalid.quarantine_policy = None,
            _ => unreachable!(),
        }
        assert!(
            render(&invalid, &invalid.identity).is_err(),
            "accepted {field}"
        );
    }
    // A newer target audit can retain crash-durable evidence for pending symbols,
    // but it cannot silently change an already completed profile's exclusions.
    let mut pending = r.clone();
    pending.complete = false;
    pending.instruments.clear();
    let next = ctrader_data_source::CrossedHistoricalQuote {
        time_ms: (r.from_utc + chrono::Duration::minutes(2)).timestamp_millis(),
        bid: 2.0,
        ask: 1.0,
    };
    audit
        .record(Minute::from_error("EURUSD", 1, &next).unwrap())
        .unwrap();
    pending.quarantine_audit = Some(audit.evidence().unwrap());
    crate::ctrader::checkpoint::validate(&pending, &pending.identity, &pending.requested_symbols)
        .unwrap();
    let mut completed = r.clone();
    completed.quarantine_audit = pending.quarantine_audit;
    assert!(
        crate::ctrader::checkpoint::validate(
            &completed,
            &completed.identity,
            &completed.requested_symbols
        )
        .is_err()
    );
    let bytes = serde_json::to_vec(&report()).unwrap();
    assert!(
        !String::from_utf8(bytes.clone())
            .unwrap()
            .contains("quarantin")
    );
    let old: Report = serde_json::from_slice(&bytes).unwrap();
    assert!(old.quarantine_policy.is_none());
}

fn audit_exclusions(r: &mut Report, path: &std::path::Path, count: usize) {
    use crate::ctrader::quarantine::{Binding, Minute, Policy, audit::Audit};
    r.quarantine_policy = Some(Policy::new(20).unwrap());
    let mut audit = Audit::open(path, None, Binding::from_report(r).unwrap()).unwrap();
    let row = &mut r.instruments[0];
    for number in 0..count {
        let crossed = ctrader_data_source::CrossedHistoricalQuote {
            time_ms: (r.from_utc + chrono::Duration::minutes(number as i64 + 1)).timestamp_millis(),
            bid: 2.0,
            ask: 1.0,
        };
        let minute = Minute::from_error(&row.measurement.symbol, row.symbol_id, &crossed).unwrap();
        audit.record(minute.clone()).unwrap();
        row.quarantined_minutes.push(minute);
    }
    r.quarantine_audit = Some(audit.evidence().unwrap());
}

#[test]
fn fractional_quarantine_limit_is_checked_for_report_resume_and_strict_bake() {
    crate::ctrader::test_logging();
    let dir = tempfile::tempdir().unwrap();
    let mut r = report();
    audit_exclusions(&mut r, &dir.path().join("audit.json"), 1);
    r.instruments[0].measurement.profile.n_bars = 999;
    assert!(render(&r, &r.identity).is_ok());
    r.instruments[0].measurement.profile.n_bars = 998;
    assert!(
        render(&r, &r.identity)
            .unwrap_err()
            .to_string()
            .contains("0.1%")
    );
    assert!(crate::ctrader::checkpoint::validate(&r, &r.identity, &r.requested_symbols).is_err());
    // A valid audit and a large explicit absolute allowance do not override the fraction.
    let mut r = report();
    audit_exclusions(&mut r, &dir.path().join("two.json"), 2);
    assert!(render(&r, &r.identity).is_err());
    r.instruments[0].measurement.profile.n_bars = 1998;
    assert!(render(&r, &r.identity).is_ok());
}

#[test]
fn sparse_stock_session_cannot_use_the_absolute_budget_to_exclude_a_large_fraction() {
    crate::ctrader::test_logging();
    let dir = tempfile::tempdir().unwrap();
    let mut r = report();
    r.identity.profile = "ctrader-demo".into();
    r.identity.account_id = 49247794;
    r.identity.broker = "Spotware".into();
    r.identity.login = 5944473;
    let native = broker_ctrader::catalogue::stock_metadata("AMP").unwrap();
    let row = &mut r.instruments[0];
    row.symbol_id = native.id;
    row.measurement.symbol = "AMP".into();
    row.measurement.pip_size = 10_f64.powi(-native.pip_position);
    row.measurement.schedule = "utc".into();
    row.measurement.timezone = "UTC".into();
    row.measurement.profile.review = ReviewStatus::MaskNotComputable;
    row.measurement.profile.n_bars = 500;
    r.requested_symbols = vec!["AMP".into()];
    audit_exclusions(&mut r, &dir.path().join("stock.json"), 1);
    assert!(
        render(&r, &r.identity)
            .unwrap_err()
            .to_string()
            .contains("0.1%")
    );
    r.instruments[0].measurement.profile.n_bars = 999;
    assert!(render(&r, &r.identity).is_ok());
}
