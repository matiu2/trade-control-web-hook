//! Shared clap definitions for the `replay-candles` binary.
//!
//! These live in the `cli` **library** (not the binary) so a second
//! consumer — `tv-arm --replay` — can build and validate a replay
//! invocation against the *same* clap struct the standalone binary
//! parses. One source of truth for the flags, their defaults, help
//! text, and validation; no arg drift between `replay-candles` and the
//! `tv-arm` pre-flight parse.
//!
//! The heavier replay machinery (candle pulling, the engine loop, the
//! report) stays bin-local under `cli/src/bin/replay_candles/`. Only the
//! CLI surface — [`ReplayArgs`] plus the value-enums its fields reference
//! ([`CandleSource`], [`DirectionFilter`], [`GoldenFilter`]) and the
//! resolved [`DetectorMarkConfig`] — is shared here.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use trade_control_core::intent::Direction;

/// Which broker candle-cache pulls (and caches) candles from. **Both** sources
/// always go through candle-cache, so either choice fills the on-disk cache and
/// reduces future broker calls — `--source` only selects the broker, never
/// whether the cache is used. The live cron engine pulls from TradeNation, so
/// that's the default: it reproduces what the engine actually saw. OANDA is
/// offered because it needs no TradeNation session; its mid prices differ
/// slightly from TradeNation's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[clap(rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum CandleSource {
    /// TradeNation candles via candle-cache (matches the live engine).
    TradeNation,
    /// OANDA v20 candles via candle-cache.
    Oanda,
}

impl CandleSource {
    /// The lower-case wire form — the value `--source` accepts and the
    /// string `tv-arm --replay` passes through when it derives the source
    /// from the resolved broker.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TradeNation => "tradenation",
            Self::Oanda => "oanda",
        }
    }
}

/// Which detected directions to mark, relative to the plan's trade direction.
/// `none` on this axis (or the golden axis) disables marking entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DirectionFilter {
    /// Only signals in the plan's trade direction — the ones that could have
    /// been entries. The default: this is the "why didn't my entry fire" view.
    With,
    /// Only signals opposite the plan's trade direction (invalidation /
    /// opposing-reversal candidates).
    Against,
    /// Both directions.
    Both,
    /// Disable direction marking (turns the whole feature off).
    None,
}

/// Which golden-ness to mark. `none` (or `none` on the direction axis) disables
/// marking entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GoldenFilter {
    /// Only golden signals (size ≥ ATR at signal time). The default.
    Golden,
    /// Only non-golden signals.
    NonGolden,
    /// Both golden and non-golden.
    Both,
    /// Disable golden marking (turns the whole feature off).
    None,
}

/// The resolved detector-marking configuration, carried into the replay loop and
/// the report. Built from the two CLI flags plus the plan's trade direction (the
/// reference the `with`/`against` filter is relative to).
#[derive(Debug, Clone, Copy)]
pub struct DetectorMarkConfig {
    pub direction: DirectionFilter,
    pub golden: GoldenFilter,
    /// The plan's trade direction — `with` means matching this, `against` means
    /// the opposite.
    pub trade_direction: Direction,
}

impl DetectorMarkConfig {
    pub fn new(
        direction: DirectionFilter,
        golden: GoldenFilter,
        trade_direction: Direction,
    ) -> Self {
        Self {
            direction,
            golden,
            trade_direction,
        }
    }

    /// True when either axis is `none`: the feature is off, no bars are marked
    /// and no summary is printed.
    pub fn is_off(&self) -> bool {
        matches!(self.direction, DirectionFilter::None) || matches!(self.golden, GoldenFilter::None)
    }

    /// Should a `needs golden but signal is not golden` entry-decline be
    /// suppressed from the report under this config?
    ///
    /// When the operator is marking golden-only candles (`--candle-detector-golden
    /// golden`, the default), a "not golden" decline is tautological noise: they
    /// already said they only care about golden signals, so telling them a
    /// non-golden signal was declined *for being non-golden* adds nothing. Any
    /// other golden setting (`non-golden` / `both`) — or the feature being off —
    /// wants the true reason, so it's kept.
    pub fn suppresses_not_golden_decline(&self) -> bool {
        matches!(self.golden, GoldenFilter::Golden)
    }

    /// Does a detected signal with this direction + golden-ness pass the filter?
    /// Always false when the feature is off.
    pub fn accepts(&self, dir: Direction, is_golden: bool) -> bool {
        if self.is_off() {
            return false;
        }
        let dir_ok = match self.direction {
            DirectionFilter::With => dir == self.trade_direction,
            DirectionFilter::Against => dir != self.trade_direction,
            DirectionFilter::Both => true,
            DirectionFilter::None => false,
        };
        let golden_ok = match self.golden {
            GoldenFilter::Golden => is_golden,
            GoldenFilter::NonGolden => !is_golden,
            GoldenFilter::Both => true,
            GoldenFilter::None => false,
        };
        dir_ok && golden_ok
    }
}

/// `replay-candles` command-line arguments. Shared between the standalone
/// binary and `tv-arm --replay`.
#[derive(Parser, Debug)]
#[command(name = "replay-candles")]
#[command(about = "Replay a candle window through the engine's decision logic, offline")]
pub struct ReplayArgs {
    /// Path to the TradePlan JSON written by `tv-arm --plan-out`. Required for a
    /// live replay; omitted (and ignored) under `--test-mode`, where the plan
    /// comes from the saved fixture.
    #[arg(long)]
    pub plan: Option<PathBuf>,

    /// Instrument to pull candles for (e.g. `eur/cad`). Overrides the chart's
    /// symbol; falls back to the TradingView chart, then the plan's instrument.
    /// Resolved per-source via instrument-lookup.
    #[arg(long)]
    pub instrument: Option<String>,

    /// Candle granularity (`1m`/`5m`/`15m`/`1h`/`4h`/`1d`). Defaults to the
    /// plan's granularity; pass this only to override it (the override must
    /// still match the plan's granularity).
    #[arg(long)]
    pub granularity: Option<String>,

    /// Which broker candle-cache pulls from. Both sources always go through
    /// candle-cache (filling the on-disk cache either way); this only selects
    /// the broker. TradeNation matches the live engine.
    #[arg(long, value_enum, default_value_t = CandleSource::TradeNation)]
    pub source: CandleSource,

    /// Window start. A bare datetime is Brisbane time (UTC+10, no DST) — the
    /// zone this tool renders every candle/fill in — e.g. `2026-06-30T17:00`.
    /// An explicit offset or `Z` is honoured (`...T07:00Z`, `...T17:00+10:00`).
    /// Overrides the chart's last-shown-candle (replay cursor). Omit to read it
    /// from the TradingView chart.
    #[arg(long)]
    pub start: Option<String>,

    /// Window end. Same time format as `--start` (bare = Brisbane, explicit
    /// offset/`Z` honoured). Overrides the plan's trade-expiry. Omit to use the
    /// plan's trade-expiry (or, if it has none, the chart's visible-region end).
    #[arg(long)]
    pub end: Option<String>,

    /// Override the tv-mcp module root used to read the chart when window flags
    /// are omitted. Defaults to the hard-coded `~/Downloads/tradingview-mcp-jackson`.
    #[arg(long)]
    pub tv_mcp_root: Option<PathBuf>,

    /// Run the fill simulator on each fired enter (default on).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub simulate: bool,

    /// Print a bar-by-bar trace of the engine's silent state changes before the
    /// fire report: phase transitions, the break-and-close stamp, and the
    /// **retest stamp** (which never fires an intent, so it's invisible in the
    /// normal report). Quiet bars are omitted. For debugging "why did/didn't the
    /// entry fire" — it shows exactly which bar armed the retest gate.
    #[arg(long, visible_alias = "all-events", default_value_t = false)]
    pub verbose: bool,

    /// Which detected-signal DIRECTIONS to mark on the report, relative to the
    /// plan's trade direction: `with` (trade direction only — the entry
    /// candidates), `against` (opposite — invalidation candidates), `both`, or
    /// `none` (disable marking). Marks EVERY qualifying candle the detector
    /// printed, whether or not the plan entered on it — the "golden candle we
    /// never entered on" debugging surface. Setting either this or
    /// `--candle-detector-golden` to `none` turns marking off entirely.
    #[arg(long, value_enum, default_value_t = DirectionFilter::With)]
    pub candle_detector_direction: DirectionFilter,

    /// Which detected-signal GOLDEN-ness to mark: `golden` (size ≥ ATR — the
    /// default), `non-golden`, `both`, or `none` (disable). Pairs with
    /// `--candle-detector-direction`; `none` on either axis turns marking off.
    #[arg(long, value_enum, default_value_t = GoldenFilter::Golden)]
    pub candle_detector_golden: GoldenFilter,

    /// After replaying, draw each *filled* position onto the live TradingView
    /// chart as a native long/short position tool (green profit zone, red stop
    /// zone) plus a small outcome label, spanning the fill bar to the exit.
    /// Prior `--annotate` drawings are cleared first (tracked by entity-id in a
    /// sidecar manifest); your hand-drawn necklines/fibs are left alone. Implies
    /// `--simulate` (annotation needs the simulated fill). Uses the same tv-mcp
    /// chart as window resolution (`--tv-mcp-root`).
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    pub annotate: bool,

    /// Also annotate *not-taken* trades — pending orders that never filled and
    /// entries the worker declined — as muted grey brackets at the fire bar. Only
    /// meaningful with `--annotate` (and implies it). Off by default, so a
    /// plain `--annotate` shows just the taken positions.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    pub annotate_unfilled: bool,

    /// Number of **real** candles to pull *before* the window start as a silent
    /// warm-up prefix. These bars seed the detector (so ATR is warm and the
    /// candle patterns have context) and prime the FSM, but fire nothing — the
    /// plan only goes live at the window start. Without this, a `needs_golden`
    /// enter can never fire (ATR never warms) and a stale veto-level touch in
    /// the warm-up span would wrongly retire the plan. 200 covers the 96-bar
    /// 15m ATR plus pattern lookback; raise it for very long-lookback configs.
    ///
    /// This is a **candle count, not a wall-clock span**: a market gap (weekend
    /// / session close) inside the naive `count × bar` estimate would yield
    /// fewer real candles, so the pull widens its look-back and retries — hopping
    /// the gap — until it has this many real candles (or hits a back-off cap).
    #[arg(long, default_value_t = 200)]
    pub warmup_bars: usize,

    /// Override the candle-cache disk cache directory.
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Print the zsh completion script to stdout and exit. Source it into your
    /// fpath (or `source <(replay-candles --print-completions)`).
    #[arg(long)]
    pub print_completions: bool,

    /// After a live replay, freeze this run's inputs (plan + the pulled candle
    /// window + resolved meta) and its outcome into `<fixtures-dir>/<NAME>/`, a
    /// golden regression case the test suite re-runs offline. Run it once a
    /// replay is producing the verdict you want.
    #[arg(long, value_name = "NAME")]
    pub save: Option<String>,

    /// A free-text note stored in the saved fixture's `meta.json` describing what
    /// the fixture is meant to model — the scenario, the bug it pins, why the
    /// verdict is what it is. Read it later if the golden ever breaks. Only used
    /// alongside `--save`; ignored otherwise.
    #[arg(long, value_name = "TEXT", requires = "save")]
    pub message: Option<String>,

    /// Replay a saved fixture **offline**: load plan + candles + meta from
    /// `<fixtures-dir>/<--fixture>/` instead of pulling from the broker (no
    /// network, no env vars, no TradingView). Requires `--fixture`.
    #[arg(long, requires = "fixture")]
    pub test_mode: bool,

    /// Name of the fixture under `<fixtures-dir>/` to replay with `--test-mode`.
    #[arg(long, value_name = "NAME")]
    pub fixture: Option<String>,

    /// Under `--test-mode`, also compare the replay's outcome against the
    /// fixture's `expected.json` and exit non-zero on any mismatch (printing the
    /// diff). The gate proof for a fixture.
    #[arg(long)]
    pub check: bool,

    /// Under `--test-mode`, recompute the outcome from the frozen plan + candles
    /// and **overwrite** the fixture's `expected.json` with it. Use to re-bless a
    /// fixture after an intended behaviour change (the new golden). Mutually
    /// exclusive with `--check` (one verifies, the other rewrites).
    #[arg(long, conflicts_with = "check")]
    pub rebless: bool,

    /// Directory holding the saved fixtures. Defaults to `replay-fixtures` at the
    /// repo root (relative to the cli crate's manifest).
    #[arg(long)]
    pub fixtures_dir: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(d: DirectionFilter, g: GoldenFilter) -> DetectorMarkConfig {
        DetectorMarkConfig::new(d, g, Direction::Long)
    }

    #[test]
    fn none_on_either_axis_is_off() {
        assert!(cfg(DirectionFilter::None, GoldenFilter::Golden).is_off());
        assert!(cfg(DirectionFilter::With, GoldenFilter::None).is_off());
        assert!(!cfg(DirectionFilter::With, GoldenFilter::Golden).is_off());
    }

    #[test]
    fn off_config_accepts_nothing() {
        let c = cfg(DirectionFilter::None, GoldenFilter::Golden);
        assert!(!c.accepts(Direction::Long, true));
        assert!(!c.accepts(Direction::Short, false));
    }

    #[test]
    fn default_with_golden_marks_only_trade_dir_golden() {
        // plan is Long; default view = with-direction golden.
        let c = cfg(DirectionFilter::With, GoldenFilter::Golden);
        assert!(c.accepts(Direction::Long, true), "long golden marked");
        assert!(
            !c.accepts(Direction::Long, false),
            "long non-golden skipped"
        );
        assert!(!c.accepts(Direction::Short, true), "short golden skipped");
    }

    #[test]
    fn against_filter_flips_direction() {
        let c = cfg(DirectionFilter::Against, GoldenFilter::Golden);
        assert!(c.accepts(Direction::Short, true), "opposite golden marked");
        assert!(
            !c.accepts(Direction::Long, true),
            "trade-dir golden skipped"
        );
    }

    #[test]
    fn both_axes_both_marks_every_signal() {
        let c = cfg(DirectionFilter::Both, GoldenFilter::Both);
        assert!(c.accepts(Direction::Long, true));
        assert!(c.accepts(Direction::Long, false));
        assert!(c.accepts(Direction::Short, true));
        assert!(c.accepts(Direction::Short, false));
    }

    #[test]
    fn non_golden_filter_selects_non_golden_only() {
        let c = cfg(DirectionFilter::Both, GoldenFilter::NonGolden);
        assert!(c.accepts(Direction::Long, false));
        assert!(!c.accepts(Direction::Long, true));
    }

    #[test]
    fn source_wire_form() {
        assert_eq!(CandleSource::TradeNation.as_str(), "tradenation");
        assert_eq!(CandleSource::Oanda.as_str(), "oanda");
    }

    #[test]
    fn suppresses_not_golden_only_under_golden_filter() {
        // golden-only view → suppress the "not golden" decline noise.
        assert!(cfg(DirectionFilter::With, GoldenFilter::Golden).suppresses_not_golden_decline());
        // any other golden setting wants the true reason.
        assert!(
            !cfg(DirectionFilter::With, GoldenFilter::NonGolden).suppresses_not_golden_decline()
        );
        assert!(!cfg(DirectionFilter::With, GoldenFilter::Both).suppresses_not_golden_decline());
        assert!(!cfg(DirectionFilter::With, GoldenFilter::None).suppresses_not_golden_decline());
    }
}
