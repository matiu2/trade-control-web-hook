//! Resume account-pinned profiles, retaining the original sampling window.
use super::{Identity, Report};
use color_eyre::{Result, eyre::ensure};
use std::collections::BTreeSet;

pub fn validate(report: &Report, identity: &Identity, symbols: &[String]) -> Result<()> {
    identity.validate_demo()?;
    ensure!(
        report.schema_version == 1
            && report.identity == *identity
            && report.source == "paired-ticks-m1",
        "resume report account or price-basis identity mismatch"
    );
    ensure!(
        report.from_utc < report.to_utc
            && report.to_utc <= chrono::Utc::now()
            && report.to_utc - report.from_utc <= chrono::Duration::days(366)
            && report.from_utc.timestamp().rem_euclid(60) == 0
            && report.to_utc.timestamp().rem_euclid(60) == 0
            && report.from_utc.timestamp_subsec_nanos() == 0
            && report.to_utc.timestamp_subsec_nanos() == 0,
        "invalid checkpoint window"
    );
    let wanted = report.requested_symbols.iter().collect::<BTreeSet<_>>();
    ensure!(
        !wanted.is_empty()
            && wanted.len() == report.requested_symbols.len()
            && wanted == symbols.iter().collect(),
        "resume report catalogue changed"
    );
    let measured = report
        .instruments
        .iter()
        .map(|r| &r.measurement.symbol)
        .collect::<BTreeSet<_>>();
    let failed = report
        .failures
        .iter()
        .map(|r| &r.symbol)
        .collect::<BTreeSet<_>>();
    ensure!(
        measured.len() == report.instruments.len()
            && measured.is_subset(&wanted)
            && failed.len() == report.failures.len()
            && failed.is_subset(&wanted)
            && failed.is_disjoint(&measured),
        "checkpoint profiles are duplicated or unexpected"
    );
    Ok(())
}
pub fn remaining(report: &Report) -> Vec<String> {
    report
        .requested_symbols
        .iter()
        .filter(|symbol| {
            !report
                .instruments
                .iter()
                .any(|r| r.measurement.symbol == **symbol)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> Identity {
        Identity {
            profile: "demo".into(),
            environment: "demo".into(),
            endpoint: ctrader_api::Environment::Demo.endpoint().into(),
            account_id: 42,
            broker: "Example".into(),
            login: 123,
        }
    }
    #[test]
    fn resume_is_account_catalogue_and_basis_pinned() {
        crate::ctrader::test_logging();
        let i = identity();
        let mut r = Report {
            schema_version: 1,
            identity: i.clone(),
            source: "paired-ticks-m1".into(),
            from_utc: "2026-07-01T00:00:00Z".parse().unwrap(),
            to_utc: "2026-10-01T00:00:00Z".parse().unwrap(),
            requested_symbols: vec!["EURUSD".into()],
            complete: false,
            excluded: vec![],
            instruments: vec![],
            failures: vec![],
        };
        validate(&r, &i, &r.requested_symbols).unwrap();
        assert_eq!(remaining(&r), ["EURUSD"]);
        r.identity.broker = "Different".into();
        assert!(validate(&r, &i, &r.requested_symbols).is_err());
        r.identity = i.clone();
        r.source = "native-bid".into();
        assert!(validate(&r, &i, &r.requested_symbols).is_err());
        r.source = "paired-ticks-m1".into();
        assert!(validate(&r, &i, &["AUDUSD".into()]).is_err());
    }
}

mod files;
pub use files::Destination;
