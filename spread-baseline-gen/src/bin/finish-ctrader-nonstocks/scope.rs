//! Pin the exact verified demo identity, native catalogue subset and evidence window.
use super::args::Args;
use broker_ctrader::{
    access::Profile,
    market_data::{ChartData, InstrumentSummary},
};
use color_eyre::{Result, eyre::ensure};
use spread_baseline_gen::ctrader::{self, Identity, Report, output::TableInfo};

pub async fn fresh(args: &Args) -> Result<Identity> {
    Ok(fresh_data(args).await?.0)
}

async fn fresh_data(args: &Args) -> Result<(Identity, ChartData)> {
    args.validate()?;
    let profile = Profile::load(&args.account)?;
    ctrader::generate::demo_config(&profile)?;
    let data = ChartData::connect_profile(&args.account, profile).await?;
    let identity = ctrader::generate::identity(&args.account, &data).await?;
    catalogue(&data.catalogue().await?, &args.only)?;
    Ok((identity, data))
}

pub async fn verify_baked(args: &Args, report: &Report, table: &str) -> Result<()> {
    let (identity, data) = fresh_data(args).await?;
    baked_report(args, &identity, report, table)?;
    for row in &report.instruments {
        let metadata = ctrader::calibration::metadata(&data, &row.measurement.symbol, true).await?;
        ctrader::bake::verify_broker_metadata(row, metadata.id, &metadata.name, metadata.pip_size)?;
    }
    Ok(())
}

fn baked_report(args: &Args, identity: &Identity, evidence: &Report, source: &str) -> Result<()> {
    table(args, identity, &ctrader::output::table_info(source)?)?;
    report(args, identity, evidence, true)?;
    ctrader::output::verify_present(source, &ctrader::bake::render(evidence, identity)?)
}

fn catalogue(rows: &[InstrumentSummary], selected: &[String]) -> Result<()> {
    for name in selected {
        let matches = rows
            .iter()
            .filter(|row| row.name == *name)
            .collect::<Vec<_>>();
        ensure!(
            matches.len() == 1 && matches[0].id > 0 && matches[0].enabled == Some(true),
            "selected native symbol {name:?} is missing, disabled or ambiguous in the current account"
        );
    }
    Ok(())
}

pub fn table(args: &Args, identity: &Identity, info: &TableInfo) -> Result<()> {
    ensure!(
        info.identity == *identity,
        "existing table is for a different verified demo account"
    );
    ensure!(
        info.window
            == format!(
                "// Genuine paired M1 ticks through candle-cache, UTC {} through {}.",
                args.from, args.to
            ),
        "existing table sampling window differs from explicit completion bounds"
    );
    let present = args
        .only
        .iter()
        .filter(|symbol| {
            info.keys
                .contains(&format!("ctrader:{}:{symbol}", args.account))
        })
        .count();
    if args.verify_baked {
        ensure!(
            present == args.only.len(),
            "--verify-baked requires every selected native symbol already present; mixed or missing coverage is refused"
        );
    } else {
        ensure!(
            present == 0,
            "--only includes an already measured native symbol; compute the missing list or use --verify-baked to verify a completed merge"
        );
    }
    Ok(())
}

pub fn report(args: &Args, identity: &Identity, report: &Report, finished: bool) -> Result<()> {
    ctrader::checkpoint::validate(report, identity, &args.only)?;
    let policy = args
        .quarantine_crossed_minutes
        .then(|| ctrader::quarantine::Policy::new(args.max_quarantined_minutes))
        .transpose()?;
    ensure!(
        report.quarantine_policy == policy,
        "completion must explicitly retain the report's crossed-minute quarantine policy"
    );
    ensure!(
        report.from_utc == args.from && report.to_utc == args.to,
        "existing report sampling window differs from explicit completion bounds"
    );
    ensure!(
        report.excluded.is_empty(),
        "explicit missing-only report contains catalogue exclusions"
    );
    if finished {
        ensure!(
            report.complete
                && report.failures.is_empty()
                && report.instruments.len() == args.only.len(),
            "every selected native symbol must complete before promotion"
        );
        ctrader::bake::render(report, identity)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn evidence() -> Report {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/validation/ctrader-demo-xpdusd-90d-report.json"
        )))
        .unwrap()
    }
    #[test]
    fn table_preflight_requires_same_identity_window_and_only_missing_keys() {
        crate::test_logging();
        let a = super::super::args::tests::args();
        let mut info = TableInfo {
            identity: evidence().identity,
            window: format!(
                "// Genuine paired M1 ticks through candle-cache, UTC {} through {}.",
                a.from, a.to
            ),
            keys: vec!["ctrader:ctrader-demo:EURUSD".into()],
        };
        let identity = info.identity.clone();
        table(&a, &identity, &info).unwrap();
        info.identity.account_id += 1;
        assert!(table(&a, &identity, &info).is_err());
        info.identity = identity.clone();
        info.window.push(' ');
        assert!(table(&a, &identity, &info).is_err());
        info.window.pop();
        info.keys.push("ctrader:ctrader-demo:XPDUSD".into());
        assert!(table(&a, &identity, &info).is_err());
    }
    #[test]
    fn resume_requires_exact_identity_scope_and_window_then_complete_quality_evidence() {
        crate::test_logging();
        let a = super::super::args::tests::args();
        let mut r = evidence();
        let identity = r.identity.clone();
        report(&a, &identity, &r, true).unwrap();
        r.complete = false;
        report(&a, &identity, &r, false).unwrap();
        assert!(report(&a, &identity, &r, true).is_err());
        r.to_utc -= chrono::Duration::minutes(1);
        assert!(report(&a, &identity, &r, false).is_err());
        r = evidence();
        r.requested_symbols.push("XAUEUR".into());
        assert!(report(&a, &identity, &r, false).is_err());
        r = evidence();
        r.identity.login += 1;
        assert!(report(&a, &identity, &r, false).is_err());
        r = evidence();
        r.instruments[0].measurement.profile.n_bars = 0;
        assert!(report(&a, &identity, &r, true).is_err());
    }

    #[test]
    fn baked_recovery_requires_complete_matching_literal_rows_and_explicit_mode() {
        crate::test_logging();
        let mut args = super::super::args::tests::args();
        let mut evidence = evidence();
        let identity = evidence.identity.clone();
        let source = ctrader::bake::render(&evidence, &identity).unwrap();
        assert!(baked_report(&args, &identity, &evidence, &source).is_err());
        args.verify_baked = true;
        baked_report(&args, &identity, &evidence, &source).unwrap();
        assert!(
            baked_report(&args, &identity, &evidence, &source.replace("    (", "  (")).is_err()
        );
        evidence.complete = false;
        assert!(baked_report(&args, &identity, &evidence, &source).is_err());
        evidence.complete = true;
        args.only.push("XAUEUR".into());
        assert!(baked_report(&args, &identity, &evidence, &source).is_err());
        let info = ctrader::output::table_info(&source).unwrap();
        assert!(table(&args, &identity, &info).is_err());
        args.verify_baked = false;
        assert!(table(&args, &identity, &info).is_err());
    }
    #[test]
    fn catalogue_rejects_absent_disabled_or_ambiguous_native_names() {
        crate::test_logging();
        let a = vec!["XPDUSD".into()];
        assert!(catalogue(&[], &a).is_err());
        let mut rows = vec![InstrumentSummary {
            id: 77,
            name: "XPDUSD".into(),
            description: None,
            enabled: Some(true),
        }];
        catalogue(&rows, &a).unwrap();
        rows[0].enabled = None;
        assert!(catalogue(&rows, &a).is_err());
        rows[0].enabled = Some(false);
        assert!(catalogue(&rows, &a).is_err());
        rows[0].enabled = Some(true);
        rows.push(rows[0].clone());
        assert!(catalogue(&rows, &a).is_err());
    }
}
