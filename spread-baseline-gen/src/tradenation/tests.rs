use super::*;

fn init() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

#[test]
fn universe_contains_every_distinct_market_including_stocks_and_variants() {
    init();
    let items = universe().unwrap();
    assert_eq!(
        items.len(),
        instrument_lookup::tradenation::assets().unwrap().len()
    );
    assert!(items.len() >= 1322);
    for symbol in ["APPLOVIN CORP", "EUR/PLN", "Spot Gold (Variable Spreads)"] {
        assert!(items.iter().any(|item| item.symbol == symbol), "{symbol}");
    }
    assert!(items.iter().all(|item| item.spread_schedule_tz.is_some()));
}

#[test]
fn merging_keeps_other_brokers_and_unmeasured_tradenation_rows() {
    init();
    let existing = "// header\npub static SPREAD_BASELINE_CANDLE: &[()] = &[\n    (\"oanda\", \"EUR_USD\", 1),\n    (\"tradenation\", \"Old\", 2),\n    (\"tradenation\", \"Updated\", 3),\n];\n";
    let incoming = "// header\npub static SPREAD_BASELINE_CANDLE: &[()] = &[\n    (\"tradenation\", \"New\", 4),\n    (\"tradenation\", \"Updated\", 5),\n];\n";
    let merged = merge_tables(existing, incoming).unwrap();
    assert!(merged.contains("(\"oanda\", \"EUR_USD\", 1)"));
    assert!(merged.contains("(\"tradenation\", \"Old\", 2)"));
    assert!(merged.contains("(\"tradenation\", \"New\", 4)"));
    assert!(merged.contains("(\"tradenation\", \"Updated\", 5)"));
    assert!(!merged.contains("(\"tradenation\", \"Updated\", 3)"));
    assert!(merge_tables("corrupt", incoming).is_err());
}

#[test]
fn insufficient_stale_and_unreviewed_measurements_never_become_defaults() {
    init();
    let to = chrono::DateTime::parse_from_rfc3339("2026-10-28T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let from = to - Duration::days(60);
    let mut profile = crate::profile_from_minutes(&[], 0.01);
    profile.review = ReviewStatus::MaskNotComputable;
    profile.baseline_median_pips = 3.0;
    profile.hour_p90_frac[10] = 0.001;
    let mut row = Measurement {
        schedule: "ny".into(),
        pip_size: 0.01,
        first_utc: from + Duration::days(2),
        last_utc: to - Duration::days(1),
        observed_days: 42,
        profile,
    };
    assert!(validate_measurement(&row, from, to).is_ok());
    row.observed_days = 9;
    assert!(validate_measurement(&row, from, to).is_err());
    row.observed_days = 42;
    row.profile.review = ReviewStatus::InsufficientData;
    assert!(validate_measurement(&row, from, to).is_err());
    row.profile.review = ReviewStatus::Reviewed;
    row.last_utc = to - Duration::days(8);
    assert!(validate_measurement(&row, from, to).is_err());
    row.last_utc = to - Duration::days(1);
    row.profile.hour_p90_frac[10] = f64::NAN;
    assert!(validate_measurement(&row, from, to).is_err());
}

#[test]
fn report_lock_prevents_concurrent_bakes_and_recovers_after_drop() {
    init();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("report.json");
    let guard = output::lock(&path).unwrap();
    assert!(output::lock(&path).is_err());
    drop(guard);
    assert!(output::lock(&path).is_ok());
}

#[test]
fn an_old_checkpoint_cannot_be_republished_as_fresh_calibration() {
    init();
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-28T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut report = Report {
        source: "test".into(),
        from_utc: now - Duration::days(60),
        to_utc: now,
        catalogue: vec![],
        measurements: BTreeMap::new(),
        failures: BTreeMap::new(),
        complete: true,
    };
    assert!(validate_window(&report, now, 60).is_ok());
    report.from_utc -= Duration::days(8);
    report.to_utc -= Duration::days(8);
    assert!(validate_window(&report, now, 60).is_err());
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("must-not-exist.rs");
    assert!(promote(&out, &report).is_err());
    assert!(!out.exists());
}

#[test]
fn distinct_reports_share_one_output_lock() {
    init();
    let dir = tempfile::tempdir().unwrap();
    let _first_report = output::lock(&dir.path().join("first.json")).unwrap();
    let _second_report = output::lock(&dir.path().join("second.json")).unwrap();
    let path = dir.path().join("candle.rs");
    let output_guard = output::lock(&path).unwrap();
    assert!(output::lock(&path).is_err());
    drop(output_guard);
    assert!(output::lock(&path).is_ok());
}

#[test]
fn sparse_stock_quotes_can_measure_a_forecast_without_ranking_volatility() {
    init();
    let bars = (0..300)
        .map(|index| crate::MinuteBar {
            utc_minute_of_day: ((index % 120) + 9 * 60) as u16,
            local_hour: (9 + (index % 120) / 60) as u8,
            spread_frac: 0.001,
            mid_close: 100.0,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        crate::profile_from_minutes(&bars, 0.01).review,
        ReviewStatus::InsufficientData
    );
    let profile = crate::compute::profile_from_sparse_minutes(&bars, 0.01);
    assert_eq!(profile.review, ReviewStatus::MaskNotComputable);
    assert_eq!(profile.elevated_hours, 0);
    assert!(profile.hour_p90_frac[9] > 0.0);
    assert!(profile.baseline_median_pips > 0.0);
    assert_eq!(profile.n_bars, 300);
    let same_hour = bars
        .into_iter()
        .map(|mut bar| {
            bar.local_hour = 9;
            bar
        })
        .collect::<Vec<_>>();
    let profile = crate::compute::profile_from_sparse_minutes(&same_hour, 0.01);
    assert_eq!(profile.review, ReviewStatus::MaskNotComputable);
    assert!(profile.hour_p90_frac[9] > 0.0);
}
