//! Explicit missing-only scope, never an implicit FX universe or rolling window.
use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use color_eyre::{Result, eyre::ensure};
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Parser)]
#[command(
    about = "Complete an explicit missing-only cTrader DEMO calibration (non-stocks, or --stocks); strict additive bake, release gates, optional staging deployment; no Git writes"
)]
pub struct Args {
    #[arg(long)]
    pub account: String,
    #[arg(long)]
    pub report: PathBuf,
    #[arg(long)]
    pub status: PathBuf,
    #[arg(long, default_value = ".")]
    pub workspace: PathBuf,
    #[arg(long, value_delimiter = ',', required = true)]
    pub only: Vec<String>,
    /// Select the separately audited native demo stock/ETF catalogue, read-only.
    #[arg(long)]
    pub stocks: bool,
    #[arg(long)]
    pub from: DateTime<Utc>,
    #[arg(long)]
    pub to: DateTime<Utc>,
    #[arg(long, default_value_t = 24, value_parser = clap::value_parser!(u32).range(1..=24))]
    pub window_hours: u32,
    #[arg(long)]
    pub deploy_staging: bool,
    /// Recover after a successful merge: verify identical baked rows and rerun gates only.
    #[arg(long)]
    pub verify_baked: bool,
}

impl Args {
    pub fn validate(&self) -> Result<()> {
        let names = self
            .only
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let audited = if self.stocks {
            broker_ctrader::catalogue::stock_symbols()?
        } else {
            broker_ctrader::catalogue::non_stock_symbols()?
        };
        ensure!(
            !names.is_empty()
                && names.len() == self.only.len()
                && names.iter().all(|name| audited.contains(name)),
            "--only must contain unique exact audited native symbols matching the selected stock/non-stock scope"
        );
        ensure!(
            self.to <= Utc::now()
                && (Duration::days(90)..=Duration::days(366)).contains(&(self.to - self.from))
                && [self.from, self.to]
                    .iter()
                    .all(|time| time.timestamp().rem_euclid(60) == 0
                        && time.timestamp_subsec_nanos() == 0),
            "explicit sampling bounds must be closed UTC minutes, 90 through 366 days"
        );
        ensure!(
            [&self.report, &self.status]
                .iter()
                .all(|path| path.extension().is_some_and(|e| e == "json")),
            "report and status must be distinct JSON artifact paths"
        );
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    pub fn args() -> Args {
        Args::try_parse_from([
            "finish-ctrader-nonstocks",
            "--account",
            "ctrader-demo",
            "--report",
            "missing.json",
            "--status",
            "missing-status.json",
            "--only",
            "XPDUSD",
            "--from",
            "2026-07-24T16:26:00Z",
            "--to",
            "2026-10-22T16:26:00Z",
        ])
        .unwrap()
    }
    #[test]
    fn stock_scope_is_explicit_native_only_and_rejects_mixed_batches() {
        crate::test_logging();
        let mut a = args();
        a.only = vec![
            "AIA".into(),
            "AMP".into(),
            "REDES ENERGETICAS NACIONAIS".into(),
        ];
        assert!(a.validate().is_err());
        a.stocks = true;
        a.validate().unwrap();
        a.only.push("EURUSD".into());
        assert!(a.validate().is_err());
    }

    #[test]
    fn explicit_scope_has_no_stocks_aliases_duplicates_or_rolling_bounds() {
        crate::test_logging();
        let mut a = args();
        a.validate().unwrap();
        assert!(!a.deploy_staging);
        assert!(!a.verify_baked);
        assert_eq!(a.window_hours, 24);
        for names in [
            vec![],
            vec!["XPDUSD", "XPDUSD"],
            vec!["AMP"],
            vec!["AAPL"],
            vec!["XAU/EUR"],
        ] {
            a.only = names.into_iter().map(String::from).collect();
            assert!(a.validate().is_err());
        }
        a = args();
        a.to += Duration::seconds(1);
        assert!(a.validate().is_err());
        a = args();
        a.report = "ctrader.rs".into();
        assert!(a.validate().is_err());
        a = args();
        a.from = a.to - Duration::days(89);
        assert!(a.validate().is_err());
    }
}
