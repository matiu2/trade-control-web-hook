use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    about = "Read-only by default: probe a verified cTrader DEMO account; --execute creates/cancels minimum-volume stop/limit orders and opens/closes one market position"
)]
pub struct Args {
    #[arg(long)]
    pub account: String,
    #[arg(long)]
    pub report: PathBuf,
    #[arg(long, default_value = "EURUSD")]
    pub instrument: String,
    /// Actually submit the displayed demo probes. Without this, only metadata and quotes are read.
    #[arg(long)]
    pub execute: bool,
    /// Probe-only cap; this does not change the stored account configuration.
    #[arg(long, default_value_t = 3)]
    pub max_open_positions: u32,
    #[arg(long, default_value_t = 500.0)]
    pub pending_distance_pips: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn execution_is_explicit_and_report_is_required() {
        crate::test_logging();
        let a = Args::try_parse_from([
            "probe",
            "--account",
            "ctrader-demo",
            "--report",
            "/tmp/probe.json",
        ])
        .unwrap();
        assert!(!a.execute);
        assert_eq!(a.instrument, "EURUSD");
        assert_eq!(a.max_open_positions, 3);
        assert!(Args::try_parse_from(["probe", "--account", "ctrader-demo", "--execute"]).is_err());
    }
}
