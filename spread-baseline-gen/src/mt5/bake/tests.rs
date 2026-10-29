use super::*;
use crate::SpreadProfile;

fn report() -> Report {
    let from = "2026-07-11T00:00:00Z".parse().unwrap();
    let to = "2026-10-09T00:00:00Z".parse().unwrap();
    let mut profile = SpreadProfile::empty(1000);
    profile.review = ReviewStatus::Reviewed;
    profile.baseline_low_pips = 0.1;
    profile.baseline_median_pips = 0.2;
    profile.baseline_high_pips = 0.5;
    profile.hour_p90_frac = [0.00002; 24];
    Report {
        schema_version: 1,
        feed: "mt5-five".into(),
        login: 123,
        server: "Demo".into(),
        from_utc: from,
        to_utc: to,
        requested_symbols: vec!["EURUSD".into()],
        complete: true,
        failures: vec![],
        instruments: vec![InstrumentReport {
            symbol: "EURUSD".into(),
            pip_size: 0.0001,
            schedule: "ny".into(),
            timezone: "America/New_York".into(),
            first_minute_utc: from,
            last_minute_utc: to - chrono::Duration::minutes(1),
            elevated_local_hours: vec![],
            profile,
        }],
    }
}

#[test]
fn completed_report_renders_an_account_key_and_real_forecast() {
    let output = render(&report(), "five", 123, "Demo").unwrap();
    assert!(output.contains("\"mt5-five\", \"mt5:five:EURUSD\", \"ny\", true"));
    assert!(output.contains("2e-5"));
    assert!(render(&report(), "other", 123, "Demo").is_err());
    assert!(render(&report(), "five", 456, "Demo").is_err());
    assert!(render(&report(), "five", 123, "Other-server").is_err());
}

#[test]
fn incomplete_thin_and_duplicate_reports_cannot_replace_defaults() {
    let mut r = report();
    r.complete = false;
    assert!(render(&r, "five", 123, "Demo").is_err());
    r.complete = true;
    r.instruments[0].profile.review = ReviewStatus::InsufficientData;
    assert!(render(&r, "five", 123, "Demo").is_err());
    let mut r = report();
    r.instruments.push(r.instruments[0].clone());
    assert!(render(&r, "five", 123, "Demo").is_err());
}

#[test]
fn invalid_forecast_and_mismatched_schedule_are_rejected() {
    let mut r = report();
    r.instruments[0].profile.hour_p90_frac[3] = f64::NAN;
    assert!(render(&r, "five", 123, "Demo").is_err());
    let mut r = report();
    r.instruments[0].timezone = "UTC".into();
    assert!(render(&r, "five", 123, "Demo").is_err());
    let mut r = report();
    r.instruments[0].profile.hour_widen_frac[3] = 0.001;
    assert!(render(&r, "five", 123, "Demo").is_err());
}

#[test]
fn reports_shorter_than_the_requested_ninety_days_are_rejected() {
    let mut r = report();
    r.from_utc = r.to_utc - chrono::Duration::days(14);
    assert!(render(&r, "five", 123, "Demo").is_err());
}

#[test]
fn finished_failures_remain_unbaked_but_checkpoints_are_refused() {
    let mut r = report();
    r.complete = false;
    r.requested_symbols.push("UNAVAILABLE".into());
    assert!(render_finished(&r, "five", 123, "Demo").is_err());
    r.failures.push(super::super::Failure {
        symbol: "UNAVAILABLE".into(),
        error: "broker tick history unavailable".into(),
    });
    let table = render_finished(&r, "five", 123, "Demo").unwrap();
    assert!(table.contains("mt5:five:EURUSD"));
    assert!(!table.contains("UNAVAILABLE"));
    r.instruments[0].profile.review = ReviewStatus::InsufficientData;
    assert!(render_finished(&r, "five", 123, "Demo").is_err());
}

fn mixed_history() -> (Report, Report) {
    let mut primary = report();
    primary.complete = false;
    primary.requested_symbols.push("GBPUSD".into());
    primary.failures.push(super::super::Failure {
        symbol: "GBPUSD".into(),
        error: "older ticks unavailable".into(),
    });
    let mut recent = report();
    recent.to_utc += chrono::Duration::days(20);
    recent.from_utc = recent.to_utc - chrono::Duration::days(6);
    recent.requested_symbols = vec!["GBPUSD".into()];
    recent.instruments[0].symbol = "GBPUSD".into();
    recent.instruments[0].first_minute_utc = recent.from_utc;
    recent.instruments[0].last_minute_utc = recent.to_utc - chrono::Duration::minutes(1);
    (primary, recent)
}

#[test]
fn explicit_short_history_fills_only_failed_symbols_and_records_real_coverage() {
    logging();
    let (primary, recent) = mixed_history();
    assert!(render(&primary, "five", 123, "Demo").is_err());
    assert!(render(&recent, "five", 123, "Demo").is_err());
    let table = render_with_short_history(&primary, &recent, "five", 123, "Demo").unwrap();
    assert!(table.contains("mt5:five:EURUSD"));
    assert!(table.contains("mt5:five:GBPUSD"));
    assert!(table.contains("Short-history GBPUSD"));
    assert!(table.contains(&recent.from_utc.to_rfc3339()));
    assert!(table.contains(&recent.to_utc.to_rfc3339()));
    assert!(table.contains("1000 M1 observations"));
    assert!(table.contains("provisional"));
    assert_eq!(primary.failures.len(), 1);
    assert!(!primary.complete);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mt5.rs");
    super::super::output::write(&path, "five", &table).unwrap();
    let merged = std::fs::read_to_string(path).unwrap();
    assert!(merged.contains("Short-history GBPUSD"));
    assert_eq!(merged.matches("mt5:five:").count(), 2);
}

#[test]
fn short_history_still_requires_identity_quality_and_five_calendar_days() {
    logging();
    let (primary, recent) = mixed_history();
    let mut wrong_account = recent.clone();
    wrong_account.login += 1;
    assert!(render_with_short_history(&primary, &wrong_account, "five", 123, "Demo").is_err());
    let mut too_short = recent.clone();
    too_short.from_utc = too_short.to_utc - chrono::Duration::days(4);
    assert!(render_with_short_history(&primary, &too_short, "five", 123, "Demo").is_err());
    let mut thin = recent.clone();
    thin.instruments[0].profile.review = ReviewStatus::InsufficientData;
    assert!(render_with_short_history(&primary, &thin, "five", 123, "Demo").is_err());
    let mut incomplete = recent.clone();
    incomplete.complete = false;
    assert!(render_with_short_history(&primary, &incomplete, "five", 123, "Demo").is_err());
    assert!(render_with_short_history(&primary, &recent, "five", 456, "Demo").is_err());
    let mut short_primary = primary.clone();
    short_primary.from_utc = short_primary.to_utc - chrono::Duration::days(20);
    assert!(render_with_short_history(&short_primary, &recent, "five", 123, "Demo").is_err());
}

#[test]
fn supplemental_history_cannot_overwrite_measured_rows_or_hide_missing_symbols() {
    logging();
    let (mut primary, recent) = mixed_history();
    let mut overwrite = recent.clone();
    overwrite.requested_symbols = vec!["EURUSD".into()];
    overwrite.instruments[0].symbol = "EURUSD".into();
    assert!(render_with_short_history(&primary, &overwrite, "five", 123, "Demo").is_err());
    primary.requested_symbols.push("USDJPY".into());
    assert!(render_with_short_history(&primary, &recent, "five", 123, "Demo").is_err());
    primary.failures.push(super::super::Failure {
        symbol: "USDJPY".into(),
        error: "missing history".into(),
    });
    assert!(render_with_short_history(&primary, &recent, "five", 123, "Demo").is_err());
}

fn logging() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}
