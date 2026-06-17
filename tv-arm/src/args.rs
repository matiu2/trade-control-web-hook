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
#[command(version = env!("GIT_VERSION"), about, long_about = None)]
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

    /// Also register the trade as ONE signed `TradePlan` with the worker's
    /// server-side engine (POSTed directly to the baked webhook), in addition
    /// to creating the TradingView alerts. Experimental / dev-only: old (TV
    /// alerts) and new (engine) paths run in parallel until the engine is
    /// proven on demo. Independent of `--create-alerts` — you can register a
    /// plan without arming TV alerts, or vice versa.
    #[arg(long)]
    pub register_plan: bool,

    /// Re-arm an existing setup: before registering the fresh plan, delete the
    /// prior registered plan for this instrument from the server-side engine
    /// (clears its `plan:` + `plan-state:` KV so the new plan starts clean and
    /// the old one stops ticking). Use after moving annotations on the chart
    /// and re-running. Only meaningful with `--register-plan`.
    ///
    /// - **`--update`** (no value): auto-resolves the target by instrument —
    ///   if exactly one plan is registered for this instrument it's deleted; if
    ///   none, it's a no-op; if more than one, it's a hard error (pass the id).
    /// - **`--update <trade-id>`**: deletes exactly that plan, no matter how
    ///   many are registered. The trade_id comes from `trade-control plan list`.
    ///
    /// Leaves TradingView alerts untouched — this reconciles only the engine
    /// plan. (tv-arm mints a fresh random trade_id each run, so without
    /// `--update` a re-arm leaves the old plan ticking until its TTL.)
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    pub update: Option<String>,

    /// Register the plan in **observe-only (shadow) mode**: the server-side
    /// engine evaluates it and advances its state exactly as a live plan, but
    /// never dispatches its fires to the broker — each would-be fire is logged
    /// instead. This is the safe way to run the engine alongside the live TV
    /// alerts on demo (Stage F gate): both see the same candles, only the TV
    /// alert places real orders, so engine-vs-alert can be diffed without
    /// double-firing. Only meaningful with `--register-plan`. Default: live.
    #[arg(long)]
    pub shadow: bool,

    /// Opt in to multi-shot entries: if the broker rejects the order
    /// (e.g. spread too wide), the worker will retry on subsequent
    /// enter-alert firings up to this many times. Default (flag
    /// absent) keeps today's single-shot behaviour. Bounded by
    /// `trade_expiry`.
    #[arg(long)]
    pub max_retries: Option<u32>,

    /// Cancel the resting entry order if it hasn't filled within this
    /// many bars (1..=5). The worker indexes the Pine-filled
    /// `next_candle_timestamp_1..5` menu with this N to derive a
    /// session-calendar-aware `cancel_at` (weekend gaps skipped). Default
    /// (flag absent) leaves the order resting until `trade_expiry`.
    /// Requires the v2 indicator that ships the menu plots.
    #[arg(long)]
    pub expiry_bars: Option<u32>,

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

    /// **Experimental, default OFF.** Make a reversal off a chart-drawn
    /// `support` / `resistance` band *also* veto the upcoming entry, not
    /// just close an open position. When set, the emitted
    /// `06-close-on-reversal` intent carries `veto_on_reversal: true`, so
    /// a reversal that lands before the entry fires blocks the trade
    /// entirely (the worker writes a `reversal` veto for the trade_id).
    /// Only takes effect when support/resistance bands are present.
    #[arg(long)]
    pub veto_on_reversal: bool,

    /// (M/W only) Raise the neckline-retracement ceiling from the
    /// default `< 40%` to `<= 50%`. A retrace deeper than 40% of the
    /// runup is a marginal double-top/bottom; pass this to arm it
    /// anyway. A retrace `> 50%` is always rejected regardless of this
    /// flag. Ignored for H&S setups.
    #[arg(long)]
    pub allow_50_pct_m_trades: bool,

    /// Override the instrument pip size baked into the enter intent. When
    /// omitted, the pip size comes from `instrument-lookup`
    /// (`asset.pip_size`) — the canonical per-instrument value (0.0001 for
    /// major FX, 0.01 for JPY pairs and gold, 1.0 for indices, etc.).
    /// Applies to both H&S and M/W enters; pass this only to force a
    /// non-catalog value.
    #[arg(long)]
    pub pip_size: Option<f64>,

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
    fn mw_flags_default_off_and_parse() {
        let args = Args::try_parse_from(["tv-arm"]).expect("parse");
        assert!(!args.allow_50_pct_m_trades);
        // No --pip-size → None → pipeline uses the catalog pip_size.
        assert_eq!(args.pip_size, None);

        let args =
            Args::try_parse_from(["tv-arm", "--allow-50-pct-m-trades", "--pip-size", "0.01"])
                .expect("parse mw flags");
        assert!(args.allow_50_pct_m_trades);
        assert_eq!(args.pip_size, Some(0.01));
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
