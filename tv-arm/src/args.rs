//! Command-line arguments for `tv-arm`.
//!
//! Port of `tv_arm_hs.py::parse_args()`. Every Python flag has a
//! one-to-one Rust equivalent, except `--dry-run` (dropped — the
//! operator iterates by re-running and deleting failed alerts by
//! hand) and `--print-completions` (now powered by `clap_complete`).
//!
//! The mutually-exclusive groups Python used (`--risk-pct` /
//! `--risk-amount`) are encoded as clap groups so a double-flag is
//! caught at parse-time.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// CLI broker selection. Mirrors `conventions::Broker` but kept
/// crate-local so the value-enum can be used in `clap` derive
/// without owning the conventions crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum BrokerArg {
    /// OANDA.
    Oanda,
    /// TradeNation.
    TradeNation,
}

impl BrokerArg {
    /// Translate to the shared `Broker` enum.
    pub fn into_conventions(self) -> trade_control_conventions::Broker {
        match self {
            Self::Oanda => trade_control_conventions::Broker::Oanda,
            Self::TradeNation => trade_control_conventions::Broker::TradeNation,
        }
    }
}

/// Arm a reversal setup from the active TradingView chart.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Broker to target. Defaults to the chart's exchange (also
    /// `TRADE_CONTROL_BROKER` env).
    #[arg(long)]
    pub broker: Option<BrokerArg>,

    /// Worker account index (e.g. `ms-oanda-1`, `ms-tn-1`). Defaults
    /// per broker; also `TRADE_CONTROL_ACCOUNT` env.
    #[arg(long, env = "TRADE_CONTROL_ACCOUNT")]
    pub account_id: Option<String>,

    /// Risk per trade as a percent of equity. Default 1.0.
    #[arg(long, group = "risk")]
    pub risk_pct: Option<f64>,

    /// Risk per trade as an absolute home-currency amount (e.g. 5 = 5
    /// AUD). Lands on `intent.risk_amount`; takes precedence over
    /// `risk_pct`.
    #[arg(long, group = "risk")]
    pub risk_amount: Option<f64>,

    /// Post the alerts to TradingView. Without this, the bundle is
    /// built and signed to disk but no alerts are POSTed.
    #[arg(long)]
    pub create_alerts: bool,

    /// Set `dry_run` on the enter intent so the worker logs the order
    /// but does not send it to the broker. Useful for first-time live
    /// runs of a new sizing path. Compatible with `--create-alerts`.
    #[arg(long)]
    pub broker_dry_run: bool,

    /// Opt in to multi-shot entries: if the broker rejects the order
    /// (e.g. spread too wide), the worker will retry on subsequent
    /// enter-alert firings up to this many times. Default (flag
    /// absent) keeps today's single-shot behaviour. Bounded by
    /// `trade_expiry`.
    #[arg(long)]
    pub max_retries: Option<u32>,

    /// Use a market order for entry instead of the default pending
    /// stop-entry at the geometry anchor. SL still anchors to
    /// geometry.
    #[arg(long)]
    pub entry_market: bool,

    /// Anchor SL to Pine's `recent_high` (shorts) / `recent_low`
    /// (longs) instead of the signal bar's own wick. Requires the v2
    /// indicator from 2026-05-26+; older indicators silently fall
    /// back to the bar extreme.
    #[arg(long)]
    pub sl_from_recent: bool,

    /// Rhai script that gates whether the worker places the entry
    /// order. Lands on the enter intent's `allow_entry`. Validated at
    /// sign-time.
    #[arg(long)]
    pub entry_filter_script: Option<String>,

    /// Drop the break-and-close prep from the bundle (no alert
    /// emitted and the entry no longer requires it).
    #[arg(long)]
    pub skip_break_and_close: bool,

    /// Drop the retest prep from the bundle.
    #[arg(long)]
    pub skip_retest: bool,

    /// Require a golden signal candle on entry. Sets
    /// `needs_golden: true` on the trade spec.
    #[arg(long)]
    pub require_golden: bool,

    /// Require a confirmed signal candle on entry. Sets
    /// `needs_confirmed: true` on the enter intent. Symmetric with
    /// `--require-golden` and independent of it — pass both for a
    /// stricter "golden AND confirmed" entry gate.
    #[arg(long)]
    pub require_confirmation: bool,

    /// Skip the automatic calendar-bars step. By default, after
    /// build-trade `tv-arm` fetches this week's forex-factory events
    /// for the chart's currency pair and arms one pause-pair + one
    /// news-pair per event.
    #[arg(long)]
    pub skip_calendar_bars: bool,

    /// Half-width of the price band around each chart-drawn
    /// `support` / `resistance` line, as a percent of the line's
    /// price. Default 0.1 (= ±0.1% of price). Ignored when no
    /// support/resistance drawings are present.
    #[arg(long, default_value_t = 0.1)]
    pub reversal_band_pct: f64,

    /// Print a zsh completion script to stdout and exit.
    #[arg(long)]
    pub print_completions: bool,

    /// Override the tv-mcp module root. Defaults to the hard-coded
    /// `~/Downloads/tradingview-mcp-jackson` path.
    #[arg(long)]
    pub tv_mcp_root: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn defaults_are_sensible() {
        let args = Args::try_parse_from(["tv-arm"]).expect("parse ok");
        assert!(!args.create_alerts);
        assert!(!args.broker_dry_run);
        assert!(!args.skip_calendar_bars);
        assert_eq!(args.reversal_band_pct, 0.1);
    }

    #[test]
    fn broker_value_enum_parses() {
        let args = Args::try_parse_from(["tv-arm", "--broker", "oanda"]).expect("parse");
        assert_eq!(args.broker, Some(BrokerArg::Oanda));
        let args = Args::try_parse_from(["tv-arm", "--broker", "tradenation"]).expect("parse tn");
        assert_eq!(args.broker, Some(BrokerArg::TradeNation));
    }

    #[test]
    fn require_confirmation_defaults_off_and_parses() {
        let args = Args::try_parse_from(["tv-arm"]).expect("parse");
        assert!(!args.require_confirmation);
        let args = Args::try_parse_from(["tv-arm", "--require-confirmation"]).expect("parse");
        assert!(args.require_confirmation);
    }

    #[test]
    fn require_golden_and_confirmation_compose() {
        // Independent gates — both flags accepted together.
        let args = Args::try_parse_from(["tv-arm", "--require-golden", "--require-confirmation"])
            .expect("parse");
        assert!(args.require_golden);
        assert!(args.require_confirmation);
    }

    #[test]
    fn risk_flags_are_mutually_exclusive() {
        let res = Args::try_parse_from(["tv-arm", "--risk-pct", "1.0", "--risk-amount", "5.0"]);
        assert!(res.is_err(), "expected parse error, got {res:?}");
    }

    #[test]
    fn cli_definition_is_valid() {
        // clap will panic on duplicate flag names or other config
        // errors at command-factory time — this catches them.
        let _cmd = Args::command();
    }

    #[test]
    fn account_id_falls_back_to_env() {
        // env-based default is configured via `env = "..."` on the
        // arg. Verify the surface (parse without env still yields
        // None; with env we'd get Some, but we don't mutate process
        // env in tests).
        let args = Args::try_parse_from(["tv-arm"]).expect("parse");
        // No env set in test process → None.
        if std::env::var_os("TRADE_CONTROL_ACCOUNT").is_none() {
            assert_eq!(args.account_id, None);
        }
    }
}
