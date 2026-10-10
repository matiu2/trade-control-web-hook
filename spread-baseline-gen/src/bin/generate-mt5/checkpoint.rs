//! Resume only account-pinned checkpoints with an unchanged sampling window.
use std::collections::BTreeSet;
use color_eyre::{Result, eyre::ensure};
use mt5_data_source::account_config::AccountConfig;
use spread_baseline_gen::mt5::Report;

pub fn validate(report: &Report, account: &str, config: &AccountConfig, symbols: &[String], days: u32) -> Result<()> {
    ensure!(report.schema_version == 1 && report.feed == format!("mt5-{account}")
        && report.login == config.login && report.server == config.server,
        "resume report account identity mismatch");
    ensure!(report.to_utc - report.from_utc == chrono::Duration::days(i64::from(days)),
        "resume report sampling window differs from --days");
    let wanted = report.requested_symbols.iter().collect::<BTreeSet<_>>();
    ensure!(wanted.len() == report.requested_symbols.len() && wanted == symbols.iter().collect(),
        "resume report catalogue differs from the current account");
    let measured = report.instruments.iter().map(|row| &row.symbol).collect::<BTreeSet<_>>();
    ensure!(measured.len() == report.instruments.len() && measured.is_subset(&wanted),
        "resume report has duplicate or unexpected profiles");
    let failed = report.failures.iter().map(|row| &row.symbol).collect::<BTreeSet<_>>();
    ensure!(failed.len() == report.failures.len() && failed.is_subset(&wanted) && failed.is_disjoint(&measured),
        "resume report has duplicate or conflicting failures");
    ensure!(report.to_utc <= chrono::Utc::now(), "resume window ends in the future");
    Ok(())
}

pub fn remaining(report: &Report) -> Vec<String> {
    report.requested_symbols.iter().filter(|symbol| !report.instruments.iter()
        .any(|row| row.symbol == **symbol)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completed_profiles_are_preserved_and_failed_symbols_are_retried() {
        let from = "2026-07-11T00:00:00Z".parse().unwrap();
        let to = "2026-10-09T00:00:00Z".parse().unwrap();
        let mut report = Report { schema_version: 1, feed: "mt5-the5ers-competition".into(),
            login: 123, server: "Demo".into(), from_utc: from, to_utc: to,
            requested_symbols: vec!["EURUSD".into(), "AUDUSD".into(), "EURCAD".into()],
            complete: false, failures: vec![spread_baseline_gen::mt5::Failure { symbol: "AUDUSD".into(), error: "timeout".into() }],
            instruments: vec![spread_baseline_gen::mt5::InstrumentReport { symbol: "EURUSD".into(),
                pip_size: 0.0001, schedule: "ny".into(), timezone: "America/New_York".into(),
                first_minute_utc: from, last_minute_utc: to - chrono::Duration::minutes(1),
                elevated_local_hours: vec![], profile: spread_baseline_gen::SpreadProfile::empty(1000) }] };
        let config = AccountConfig { login: report.login, server: report.server.clone(),
            server_time: "+03:00".into(), credentials_file: "/unused/creds".into(), mailbox: "/unused/mailbox".into() };
        assert_eq!(remaining(&report), ["AUDUSD", "EURCAD"]);
        assert!(!remaining(&report).contains(&"EURUSD".into()));
        assert!(remaining(&report).contains(&"AUDUSD".into()));
        validate(&report, "the5ers-competition", &config, &report.requested_symbols, 90).unwrap();
        assert!(validate(&report, "other", &config, &report.requested_symbols, 90).is_err());
        assert!(validate(&report, "the5ers-competition", &config, &report.requested_symbols, 89).is_err());
        report.instruments.push(report.instruments[0].clone());
        assert!(validate(&report, "the5ers-competition", &config, &report.requested_symbols, 90).is_err());
    }
}
