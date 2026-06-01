//! Command-line arguments for `tv-news`.
//!
//! Deliberately minimal: the chart symbol and visible window come from
//! tv-mcp, the news currencies come from `instrument-lookup`, and the
//! star-rating filter is fixed (2★+3★ for the asset's currencies, 3★
//! USD always). The remaining knobs are operator-side overrides for
//! the tv-mcp install path and the dedupe / dry-run behaviour.

use std::path::PathBuf;

use clap::Parser;

/// Annotate the active TradingView chart with vertical-line pairs for
/// upcoming forex-factory news events.
#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Print the plan but draw nothing. Useful for sanity-checking
    /// what events would land on the chart.
    #[arg(long)]
    pub dry_run: bool,

    /// Tolerance (in minutes) for matching an existing chart drawing
    /// against a candidate event. Pairs within this window are
    /// considered duplicates and skipped. Default 5.
    #[arg(long, default_value_t = 5)]
    pub dedupe_tolerance_min: i64,

    /// Width of the `news-start` → `news-end` window, in minutes.
    /// Matches `trade-calendar-maker`'s buffer-after value (1h) by
    /// default. Bumping this widens the window the downstream
    /// `tv_extract_*_trade.py` scripts treat as "news-affected".
    #[arg(long, default_value_t = 60)]
    pub news_window_min: i64,

    /// Override the tv-mcp module root. Defaults to the hard-coded
    /// `~/Downloads/tradingview-mcp-jackson` path.
    #[arg(long)]
    pub tv_mcp_root: Option<PathBuf>,

    /// Print a zsh completion script to stdout and exit.
    #[arg(long)]
    pub print_completions: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn defaults_are_sensible() {
        let args = Args::try_parse_from(["tv-news"]).expect("parse ok");
        assert!(!args.dry_run);
        assert_eq!(args.dedupe_tolerance_min, 5);
        assert_eq!(args.news_window_min, 60);
        assert_eq!(args.tv_mcp_root, None);
    }

    #[test]
    fn news_window_min_overrides() {
        let args = Args::try_parse_from(["tv-news", "--news-window-min", "30"]).expect("parse");
        assert_eq!(args.news_window_min, 30);
    }

    #[test]
    fn cli_definition_is_valid() {
        let _cmd = Args::command();
    }

    #[test]
    fn dry_run_flag_parses() {
        let args = Args::try_parse_from(["tv-news", "--dry-run"]).expect("parse");
        assert!(args.dry_run);
    }

    #[test]
    fn dedupe_tolerance_overrides() {
        let args =
            Args::try_parse_from(["tv-news", "--dedupe-tolerance-min", "15"]).expect("parse");
        assert_eq!(args.dedupe_tolerance_min, 15);
    }
}
