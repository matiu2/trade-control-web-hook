use chrono::{DateTime, Utc};
use clap::Parser;
use std::path::PathBuf;
#[derive(Parser)]
#[command(
    about = "Measure cTrader DEMO instrument spreads from genuine paired cached M1 ticks (read-only)"
)]
pub struct Args {
    #[arg(long)]
    pub account: String,
    #[arg(long, default_value_t=90, value_parser=clap::value_parser!(u32).range(1..=366))]
    pub days: u32,
    /// Bounded cache requests; larger windows amortize native/seed calls.
    #[arg(long, default_value_t=24, value_parser=clap::value_parser!(u32).range(1..=24))]
    pub window_hours: u32,
    /// Explicit UTC bounds must end on closed minutes; default end is the current closed minute.
    #[arg(long, conflicts_with = "resume")]
    pub from: Option<DateTime<Utc>>,
    #[arg(long, conflicts_with = "resume")]
    pub to: Option<DateTime<Utc>>,
    /// Native broker symbols; default is all enabled, recognized spot-FX pairs.
    /// Explicit selection permits every catalogued class with a verified spread schedule.
    #[arg(long, value_delimiter = ',', conflicts_with = "resume")]
    pub only: Vec<String>,
    /// Discover enabled, recognized non-stock instruments with usable schedule metadata.
    /// Missing mappings/schedules are retained as explicit exclusions in the report.
    #[arg(long, conflicts_with_all = ["only", "resume"])]
    pub all_instruments: bool,
    /// Exclude and durably audit at most five crossed whole M1 minutes per instrument.
    /// Runtime feeds remain strict; every other historical-data failure stays fatal.
    #[arg(long)]
    pub quarantine_crossed_minutes: bool,
    #[arg(long, default_value_t=5, value_parser=clap::value_parser!(u32).range(1..=5), requires="quarantine_crossed_minutes")]
    pub max_quarantined_minutes: u32,
    #[arg(long)]
    pub out: PathBuf,
    /// Preserve complete symbols and use cached windows to resume interrupted symbols.
    #[arg(long)]
    pub resume: Option<PathBuf>,
    /// Authenticate and list the intended sampling workload without fetching historical ticks.
    #[arg(long)]
    pub plan: bool,
}

impl Args {
    pub(super) fn quarantine_policy(
        &self,
    ) -> color_eyre::Result<Option<crate::ctrader::quarantine::Policy>> {
        self.quarantine_crossed_minutes
            .then(|| crate::ctrader::quarantine::Policy::new(self.max_quarantined_minutes))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_instruments_is_explicit_and_cannot_compete_with_symbol_or_resume_selection() {
        crate::ctrader::test_logging();
        let base = [
            "generate-ctrader",
            "--account",
            "ctrader-demo",
            "--out",
            "/tmp/report.json",
        ];
        let all =
            Args::try_parse_from(base.into_iter().chain(["--all-instruments", "--plan"])).unwrap();
        assert!(all.all_instruments);
        assert!(!Args::try_parse_from(base).unwrap().all_instruments);
        for tail in [
            vec!["--all-instruments", "--only", "XAUEUR"],
            vec!["--all-instruments", "--resume", "/tmp/old.json"],
        ] {
            assert!(Args::try_parse_from(base.into_iter().chain(tail)).is_err());
        }
    }

    #[test]
    fn quarantine_is_explicit_and_hard_bounded() {
        crate::ctrader::test_logging();
        let base = [
            "generate-ctrader",
            "--account",
            "demo",
            "--out",
            "/tmp/report.json",
        ];
        assert!(
            Args::try_parse_from(base)
                .unwrap()
                .quarantine_policy()
                .unwrap()
                .is_none()
        );
        let args =
            Args::try_parse_from(base.into_iter().chain(["--quarantine-crossed-minutes"])).unwrap();
        assert_eq!(
            args.quarantine_policy()
                .unwrap()
                .unwrap()
                .max_minutes_per_instrument,
            5
        );
        for tail in [
            vec!["--max-quarantined-minutes", "1"],
            vec![
                "--quarantine-crossed-minutes",
                "--max-quarantined-minutes",
                "0",
            ],
            vec![
                "--quarantine-crossed-minutes",
                "--max-quarantined-minutes",
                "6",
            ],
        ] {
            assert!(Args::try_parse_from(base.into_iter().chain(tail)).is_err());
        }
    }
    #[test]
    fn default_is_all_supported_fx_with_ninety_days_and_dry_plan_available() {
        crate::ctrader::test_logging();
        let args = Args::try_parse_from([
            "generate-ctrader",
            "--account",
            "ctrader-demo",
            "--out",
            "/tmp/report.json",
            "--plan",
        ])
        .unwrap();
        assert!(args.only.is_empty());
        assert_eq!(args.days, 90);
        assert_eq!(args.window_hours, 24);
        assert!(args.plan);
        assert!(
            Args::try_parse_from([
                "generate-ctrader",
                "--account",
                "ctrader-demo",
                "--out",
                "/tmp/report.json",
                "--days",
                "0"
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "generate-ctrader",
                "--account",
                "ctrader-demo",
                "--out",
                "/tmp/report.json",
                "--resume",
                "/tmp/old.json",
                "--only",
                "EURUSD"
            ])
            .is_err()
        );
    }
}
