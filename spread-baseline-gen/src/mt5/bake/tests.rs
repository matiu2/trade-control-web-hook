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
