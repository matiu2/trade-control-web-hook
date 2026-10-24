//! A completion job must cover the freshly verified full supported FX catalogue.
use super::*;
use crate::ctrader::Identity;

/// Reauthenticate and reject subset/stale-catalogue reports before strict baking.
pub async fn verify_full_universe(account: &str, report: &Report) -> Result<()> {
    let profile = Profile::load(account)?;
    demo_config(&profile)?;
    let data = ChartData::connect_profile(account, profile).await?;
    let identity = identity(account, &data).await?;
    let (requested, _) = super::universe::select(&data, &[], false).await?;
    validate(report, &identity, &requested)
}

fn validate(report: &Report, identity: &Identity, requested: &[String]) -> Result<()> {
    crate::ctrader::checkpoint::validate(report, identity, requested).map_err(|error| {
        error.wrap_err("completion requires the current account's entire supported FX catalogue")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subset_checkpoint_cannot_pass_full_catalogue_completion() {
        crate::ctrader::test_logging();
        let identity = Identity {
            profile: "demo".into(),
            environment: "demo".into(),
            endpoint: ctrader_api::Environment::Demo.endpoint().into(),
            account_id: 42,
            broker: "Example".into(),
            login: 123,
        };
        let mut report = Report {
            schema_version: 1,
            identity: identity.clone(),
            source: "paired-ticks-m1".into(),
            from_utc: "2026-07-01T00:00:00Z".parse().unwrap(),
            to_utc: "2026-10-01T00:00:00Z".parse().unwrap(),
            requested_symbols: vec!["EURUSD".into()],
            complete: false,
            excluded: vec![],
            instruments: vec![],
            failures: vec![],
        };
        let full = vec!["AUDUSD".into(), "EURUSD".into()];
        assert!(validate(&report, &identity, &full).is_err());
        report.requested_symbols = full.clone();
        assert!(validate(&report, &identity, &full).is_ok());
        assert!(validate(&report, &identity, &["EURUSD".into()]).is_err());
        report.requested_symbols.push("EURUSD".into());
        assert!(validate(&report, &identity, &full).is_err());
        report.requested_symbols = full;
        report.identity.account_id += 1;
        assert!(validate(&report, &identity, &report.requested_symbols).is_err());
    }
}
