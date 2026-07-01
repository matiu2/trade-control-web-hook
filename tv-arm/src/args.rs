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

/// Market-hours blackout close policy. Crate-local so the value-enum can be
/// used in the `clap` derive; maps to
/// [`trade_control_core::intent::BlackoutCloseAction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum BlackoutClose {
    /// Cancel the unfilled resting order only; never close a position. Default.
    Cancel,
    /// Cancel the resting order **and** market-close an open position.
    Close,
}

impl BlackoutClose {
    /// Translate to the signed wire enum.
    pub fn into_core(self) -> trade_control_core::intent::BlackoutCloseAction {
        match self {
            Self::Cancel => trade_control_core::intent::BlackoutCloseAction::CancelResting,
            Self::Close => trade_control_core::intent::BlackoutCloseAction::CancelAndClose,
        }
    }
}

/// What to do when an H&S / iH&S stop entry goes wrong-side at resolve
/// time (the breakout ran during the signal-confirmation wait). Crate-local
/// so the value-enum works in the `clap` derive; maps to
/// [`trade_control_core::intent::RecoverEntryAction`]. The CLI vocabulary
/// (`market | limit | abort`) maps `abort → Skip` (the wire variant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum RecoverEntry {
    /// Enter the confirmed breakout at market (bounded by the SL→entry
    /// distance).
    Market,
    /// Rest a limit at the original trigger and wait for the pullback —
    /// preserves the planned R exactly.
    Limit,
    /// Drop the entry (today's behaviour for an un-opted stop).
    Abort,
}

impl RecoverEntry {
    /// Translate to the signed wire action (`abort → Skip`).
    pub fn into_core(self) -> trade_control_core::intent::RecoverEntryAction {
        use trade_control_core::intent::RecoverEntryAction as Core;
        match self {
            Self::Market => Core::Market,
            Self::Limit => Core::Limit,
            Self::Abort => Core::Skip,
        }
    }
}

/// Which order type a position-tool direct entry should place. Only one
/// of `--market-entry` / `--stop-entry` / `--limit-entry` may be given;
/// they're mutually exclusive (the `position_entry` clap group enforces
/// it). Set by [`Args::position_entry_mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEntry {
    /// Enter at market immediately (worker fills at broker bid/ask).
    Market,
    /// Rest a stop order at the drawing's entry price.
    Stop,
    /// Rest a limit order at the drawing's entry price.
    Limit,
}

/// Arm a reversal setup from the active TradingView chart.
#[derive(Debug, Parser)]
#[command(version = env!("GIT_VERSION"), about, long_about = None)]
#[command(group(
    clap::ArgGroup::new("position_entry")
        .args(["market_entry", "stop_entry", "limit_entry"])
        .multiple(false)
))]
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

    /// Set `dry_run` on the enter intent so the worker logs the order
    /// but does not send it to the broker. Useful for first-time live
    /// runs of a new sizing path.
    #[arg(long)]
    pub broker_dry_run: bool,

    /// Register the trade as ONE signed `TradePlan` with the worker's
    /// server-side engine (POSTed directly to the baked webhook). This is
    /// how a trade is armed: the `*/15` cron then evaluates the plan against
    /// fresh candles and dispatches its fires. (The legacy path — POST a
    /// signed alert bundle to TradingView and let TV fire the alerts — has
    /// been retired.)
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
    /// instead. The safe way to watch a new plan's decisions on demo without
    /// placing real orders. Only meaningful with `--register-plan`. Default: live.
    #[arg(long)]
    pub shadow: bool,

    /// Write the built `TradePlan` as pretty JSON to this path. Lets the offline
    /// `replay-candles` harness load the exact plan the engine would receive and
    /// replay a candle window through it.
    ///
    /// Builds the plan on its own — you do **not** need `--register-plan`. Used
    /// alone, it writes the JSON and stops (no worker POST). Combined with
    /// `--register-plan`, it also registers the plan with the worker.
    #[arg(long)]
    pub plan_out: Option<PathBuf>,

    /// Opt in to multi-shot entries: if the broker rejects the order
    /// (e.g. spread too wide), the worker will retry on subsequent
    /// enter-alert firings up to this many times. Defaults to 5 when
    /// the flag is absent. Pass `--max-retries 0` for single-shot.
    /// Bounded by `trade_expiry`.
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

    /// What the market-hours blackout sweep should do with this trade's
    /// resting entry order if it's caught inside the instrument's daily
    /// close→open gap. `cancel` (default) cancels the unfilled order and
    /// never touches a filled position; `close` also market-closes any open
    /// position on the instrument. Lands on the enter intent's
    /// `blackout_close`.
    #[arg(long, value_enum, default_value_t = BlackoutClose::Cancel)]
    pub blackout_close: BlackoutClose,

    /// Use a market order for entry instead of the default pending
    /// stop-entry at the geometry anchor. SL still anchors to
    /// geometry. (Pattern path — H&S / M/W.)
    #[arg(long)]
    pub entry_market: bool,

    /// **Position-tool direct entry.** Read the long/short *position*
    /// tool drawn on the chart and place a **market** order immediately
    /// (worker fills at broker price on receipt), with the drawing's
    /// entry / SL / TP. Mutually exclusive with `--stop-entry` /
    /// `--limit-entry`. No pattern, preps, or geometry needed — just the
    /// drawn position + a trade-expiry.
    #[arg(long)]
    pub market_entry: bool,

    /// **Position-tool direct entry.** Rest a **stop** order at the
    /// drawn position's entry price. Mutually exclusive with
    /// `--market-entry` / `--limit-entry`.
    #[arg(long)]
    pub stop_entry: bool,

    /// **Position-tool direct entry.** Rest a **limit** order at the
    /// drawn position's entry price. Mutually exclusive with
    /// `--market-entry` / `--stop-entry`.
    #[arg(long)]
    pub limit_entry: bool,

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

    /// **Quasimodo setup.** Convenience alias that expands to
    /// `--skip-break-and-close --skip-retest --require-confirmation`:
    /// drop both H&S preps and gate the entry on a confirmed signal
    /// candle instead. Combines with the underlying flags (it only
    /// *adds* — passing one of them as well is harmless).
    #[arg(long)]
    pub quasimodo: bool,

    /// **strategy-v2 (H&S only).** Arm a *second* entry — the Quasimodo
    /// limit — alongside the normal break-and-close + retest stop entry, on
    /// the same setup. The QM entry drops both preps, is gated only on a
    /// confirmed signal candle, and rests as a limit order at the same signal
    /// level the stop entry anchors to (filling on the pullback back to the
    /// level rather than a break through it). Whichever fires first wins: the
    /// worker cancels the other's resting order, and an already-open position
    /// blocks the sibling. The stop entry wins a same-bar tie.
    ///
    /// This is NOT `--quasimodo` (which runs the QM setup *instead of* the
    /// stop entry) — strategy-v2 runs both. Conflicts with `--quasimodo`,
    /// `--entry-market`, `--skip-break-and-close`, and `--skip-retest`;
    /// `--max-retries 0` is rejected at validation.
    #[arg(
        long,
        conflicts_with_all = ["quasimodo", "entry_market", "skip_break_and_close", "skip_retest"]
    )]
    pub strategy_v2: bool,

    /// Disable break-even stop management for this trade. By default the
    /// `05-enter` carries a break-even rule (50% of entry→TP) so the live
    /// worker moves the stop to break-even once a candle closes past the
    /// midpoint (BUG-replay-no-breakeven-stop-at-50pct). Pass this to opt out
    /// (the stop stays at its original level for the life of the position).
    #[arg(long)]
    pub no_breakeven: bool,

    /// Override the break-even arm threshold as a fraction of entry→TP
    /// (default 0.5 = 50%). Ignored when `--no-breakeven` is set. Values
    /// outside `(0, 1)` are clamped to 0.5 by the worker/replay.
    #[arg(long)]
    pub breakeven_pct: Option<f64>,

    /// Drop the break-and-close prep from the bundle (no alert
    /// emitted and the entry no longer requires it).
    #[arg(long)]
    pub skip_break_and_close: bool,

    /// Drop the retest prep from the bundle.
    #[arg(long)]
    pub skip_retest: bool,

    /// Drop the golden-signal-candle requirement on entry. By default
    /// a golden signal candle is required (`needs_golden: true` on the
    /// trade spec); pass this to clear it.
    #[arg(long)]
    pub skip_golden: bool,

    /// Require a confirmed signal candle on entry. Sets
    /// `needs_confirmed: true` on the enter intent. Independent of the
    /// golden gate (which is on by default; clear it with
    /// `--skip-golden`) — leave golden on and pass this for a stricter
    /// "golden AND confirmed" entry gate.
    #[arg(long)]
    pub require_confirmation: bool,

    /// How to recover an H&S / iH&S stop entry that has gone wrong-side by
    /// the time the signal confirms (price broke through the trigger during
    /// the confirmation wait). `market` enters the breakout at market;
    /// `limit` rests at the original trigger for the pullback (preserves R);
    /// `abort` drops it. When omitted the default is keyed off
    /// `--require-confirmation`: a confirmation-required setup (which
    /// introduces the very lag that strands the stop) defaults to `limit`;
    /// otherwise the default is to drop (today's behaviour). H&S / iH&S
    /// only — M/W is unaffected. The ≥1R and SL≥10×spread floors still gate
    /// the recovered entry.
    #[arg(long, value_enum)]
    pub recover_entry: Option<RecoverEntry>,

    /// Skip the automatic calendar-bars step. By default, after
    /// build-trade `tv-arm` fetches this week's forex-factory events
    /// for the chart's currency pair and arms one pause-pair + one
    /// news-pair per event.
    #[arg(long)]
    pub skip_calendar_bars: bool,

    /// Override the "as-of" time used to prune already-elapsed news /
    /// blackout control pairs (RFC3339, e.g. `2026-05-28T21:00:00Z`).
    ///
    /// By default an offline `--plan-out` build prunes against the chart's
    /// replay-cursor (the visible range's right edge) so a historical replay
    /// keeps blackouts that are still *upcoming* relative to the cursor; a
    /// live `--register-plan` build prunes against wall-clock now. This flag
    /// forces an explicit cursor for headless / cron replays where no live
    /// chart range is readable. Ignored on the `--register-plan` live path.
    #[arg(long)]
    pub as_of: Option<String>,

    /// Treat this timestamp as "live now" and find the setup's drawings by
    /// searching the **whole chart** (nearest-to-start), ignoring the visible
    /// window (RFC3339, e.g. `2026-06-15T22:00:00Z`).
    ///
    /// The journaling use-case: put TradingView in replay mode with the last
    /// visible candle mid-right-shoulder, but you no longer have to *hide* the
    /// future candles — pass `--start <shoulder-time>` and tv-arm walks the
    /// chart to find each role relative to that cursor instead of relying on
    /// what's on screen:
    /// - **H&S neckline** (break-and-close + retest): the nearest trendline
    ///   *before* `--start`.
    /// - **invalidation** (`too-low` / `too-high`): the nearest horizontal to
    ///   `--start` (brackets the pattern).
    /// - **M/W path**: the path whose two shoulders bracket `--start`
    ///   (`B left-shoulder <= start <= D right-shoulder`; when the right
    ///   shoulder isn't drawn, `start >= B` — it's still forming).
    /// - **trade-expiry**: the nearest vertical *after* `--start`.
    /// - **calendar / news bars**: auto-drawn over `[--start, trade-expiry]`.
    ///
    /// Also sets the prune cursor (like `--as-of`) to `--start`, so elapsed
    /// news/blackout pairs are pruned relative to it. Absent: unchanged — the
    /// visible window scopes discovery and the replay cursor is the loaded-bars
    /// right edge. Intended for offline `--plan-out` journaling; on the live
    /// `--register-plan` path it still overrides discovery + cursor if set.
    #[arg(long)]
    pub start: Option<String>,

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

    /// (Position-tool entry only) Trade-expiry window in hours from now,
    /// used when the chart carries no `trade-expiry` vertical line. The
    /// emitted enter self-cancels (if still resting) / the setup expires
    /// at `now + this`. Ignored when a `trade-expiry` line is present
    /// (the line wins). Default 48h.
    #[arg(long, default_value_t = 48)]
    pub expiry_hours: u32,

    /// Print a zsh completion script to stdout and exit.
    #[arg(long)]
    pub print_completions: bool,

    /// Override the tv-mcp module root. Defaults to the hard-coded
    /// `~/Downloads/tradingview-mcp-jackson` path.
    #[arg(long)]
    pub tv_mcp_root: Option<PathBuf>,
}

impl Args {
    /// Expand convenience aliases into the concrete flags the pipeline
    /// reads. `--quasimodo` is shorthand for `--skip-break-and-close
    /// --skip-retest --require-confirmation`; it only ORs the targets on
    /// (never clears them), so combining it with the underlying flags is
    /// harmless. Call once after `parse`, before the pipeline runs.
    pub fn apply_aliases(mut self) -> Self {
        if self.quasimodo {
            self.skip_break_and_close = true;
            self.skip_retest = true;
            self.require_confirmation = true;
        }
        self
    }

    /// Validate flag combinations clap can't express. Call after
    /// `apply_aliases`, before the pipeline runs. Currently: `--strategy-v2`
    /// needs a non-zero `max_retries` (it's the multi_shot flag that keeps the
    /// engine plan alive so the worker can cancel the losing enter's resting
    /// order). The mutual exclusions with `--quasimodo` / `--entry-market` /
    /// `--skip-*` are enforced by clap's `conflicts_with_all` at parse time.
    pub fn validate(&self) -> color_eyre::eyre::Result<()> {
        if self.strategy_v2 && self.max_retries == Some(0) {
            color_eyre::eyre::bail!(
                "--strategy-v2 requires a non-zero --max-retries: both enters \
                 must be multi-shot so the worker can cancel the sibling's \
                 resting order when one fires (omit --max-retries to use the \
                 default of 5)"
            );
        }
        Ok(())
    }

    /// The selected position-tool entry mode, or `None` when none of the
    /// three flags is set (the normal pattern-arming path). The clap
    /// group guarantees at most one is set.
    pub fn position_entry_mode(&self) -> Option<PositionEntry> {
        match (self.market_entry, self.stop_entry, self.limit_entry) {
            (true, _, _) => Some(PositionEntry::Market),
            (_, true, _) => Some(PositionEntry::Stop),
            (_, _, true) => Some(PositionEntry::Limit),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn defaults_are_sensible() {
        let args = Args::try_parse_from(["tv-arm"]).expect("parse ok");
        assert!(!args.broker_dry_run);
        assert!(!args.skip_calendar_bars);
        assert_eq!(args.reversal_band_pct, 0.1);
    }

    #[test]
    fn blackout_close_defaults_to_cancel_and_parses() {
        use trade_control_core::intent::BlackoutCloseAction;
        // Default (flag absent) is the safe incident-fix policy.
        let args = Args::try_parse_from(["tv-arm"]).expect("parse");
        assert_eq!(args.blackout_close, BlackoutClose::Cancel);
        assert_eq!(
            args.blackout_close.into_core(),
            BlackoutCloseAction::CancelResting
        );
        // `--blackout-close close` opts into also flattening an open position.
        let args =
            Args::try_parse_from(["tv-arm", "--blackout-close", "close"]).expect("parse close");
        assert_eq!(args.blackout_close, BlackoutClose::Close);
        assert_eq!(
            args.blackout_close.into_core(),
            BlackoutCloseAction::CancelAndClose
        );
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
    fn recover_entry_is_optional_and_maps_to_core() {
        use trade_control_core::intent::RecoverEntryAction as Core;
        // Absent → None (the caller derives the default from
        // `--require-confirmation`).
        let args = Args::try_parse_from(["tv-arm"]).expect("parse");
        assert_eq!(args.recover_entry, None);
        // Each value parses and maps to the wire action (abort → Skip).
        for (flag, want) in [
            ("market", Core::Market),
            ("limit", Core::Limit),
            ("abort", Core::Skip),
        ] {
            let args =
                Args::try_parse_from(["tv-arm", "--recover-entry", flag]).expect("parse recover");
            assert_eq!(args.recover_entry.unwrap().into_core(), want, "flag {flag}");
        }
    }

    #[test]
    fn quasimodo_expands_to_the_three_flags() {
        // Bare --quasimodo is off until apply_aliases runs.
        let parsed = Args::try_parse_from(["tv-arm", "--quasimodo"]).expect("parse");
        assert!(parsed.quasimodo);
        assert!(!parsed.skip_break_and_close);
        assert!(!parsed.skip_retest);
        assert!(!parsed.require_confirmation);

        // After expansion all three concrete flags are on.
        let args = parsed.apply_aliases();
        assert!(args.skip_break_and_close);
        assert!(args.skip_retest);
        assert!(args.require_confirmation);
    }

    #[test]
    fn strategy_v2_parses_and_defaults_off() {
        let args = Args::try_parse_from(["tv-arm"]).expect("parse");
        assert!(!args.strategy_v2);
        let args = Args::try_parse_from(["tv-arm", "--strategy-v2"]).expect("parse");
        assert!(args.strategy_v2);
        // Default max_retries (omitted) is valid under strategy-v2.
        args.validate().expect("default max_retries validates");
    }

    #[test]
    fn strategy_v2_conflicts_with_quasimodo_and_friends() {
        for conflicting in [
            "--quasimodo",
            "--entry-market",
            "--skip-break-and-close",
            "--skip-retest",
        ] {
            let res = Args::try_parse_from(["tv-arm", "--strategy-v2", conflicting]);
            assert!(
                res.is_err(),
                "--strategy-v2 must conflict with {conflicting}"
            );
        }
    }

    #[test]
    fn strategy_v2_rejects_zero_max_retries() {
        let args = Args::try_parse_from(["tv-arm", "--strategy-v2", "--max-retries", "0"])
            .expect("parse")
            .apply_aliases();
        assert!(
            args.validate().is_err(),
            "--strategy-v2 with --max-retries 0 must be rejected"
        );
        // A positive value is fine.
        let args = Args::try_parse_from(["tv-arm", "--strategy-v2", "--max-retries", "3"])
            .expect("parse")
            .apply_aliases();
        args.validate().expect("positive max_retries validates");
    }

    #[test]
    fn apply_aliases_is_a_noop_without_quasimodo() {
        let args = Args::try_parse_from(["tv-arm", "--skip-retest"])
            .expect("parse")
            .apply_aliases();
        assert!(!args.quasimodo);
        // Only the explicitly-passed flag is set; the others stay off.
        assert!(args.skip_retest);
        assert!(!args.skip_break_and_close);
        assert!(!args.require_confirmation);
    }

    #[test]
    fn golden_default_on_skip_clears_it() {
        // Golden is required by default; --skip-golden clears it.
        let args = Args::try_parse_from(["tv-arm"]).expect("parse");
        assert!(!args.skip_golden);

        let args = Args::try_parse_from(["tv-arm", "--skip-golden", "--require-confirmation"])
            .expect("parse");
        assert!(args.skip_golden);
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
    fn position_entry_flags_resolve() {
        let args = Args::try_parse_from(["tv-arm"]).expect("parse");
        assert_eq!(args.position_entry_mode(), None);
        assert_eq!(args.expiry_hours, 48);

        let m = Args::try_parse_from(["tv-arm", "--market-entry"]).expect("parse");
        assert_eq!(m.position_entry_mode(), Some(PositionEntry::Market));
        let s = Args::try_parse_from(["tv-arm", "--stop-entry"]).expect("parse");
        assert_eq!(s.position_entry_mode(), Some(PositionEntry::Stop));
        let l = Args::try_parse_from(["tv-arm", "--limit-entry"]).expect("parse");
        assert_eq!(l.position_entry_mode(), Some(PositionEntry::Limit));
    }

    #[test]
    fn position_entry_flags_are_mutually_exclusive() {
        let res = Args::try_parse_from(["tv-arm", "--market-entry", "--stop-entry"]);
        assert!(res.is_err(), "expected parse error, got {res:?}");
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
