//! Shared clap definitions for the `replay-candles` binary.
//!
//! These live in the `cli` **library** (not the binary) so a second
//! consumer — `tv-arm ... replay` — can build and validate a replay
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
    /// string `tv-arm ... replay` passes through when it derives the source
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

/// Exit-code contract, shown under `--help`. Batch drivers branch on these, so
/// they are part of the interface: changing a number is a breaking change.
const EXIT_CODE_HELP: &str = "\
EXIT CODES:
  0  the replay ran to completion — record the result, whatever it was
     (including a legitimate no-fill 0R)
  2  usage error (clap): an unknown or malformed flag
  3  infrastructure failure — candle cache unreachable, broker auth/network.
     Nothing was measured; retry it
  4  bad input — unparseable window, missing plan, no such fixture.
     Retrying verbatim will fail identically; fix the input
  5  --check ran and the fixture did not match expected.json. A regression
     verdict, not a fault — do not retry; investigate or re-bless

A terminal summary line is ALWAYS printed to stdout, success or failure, so its
absence means the process died in a way nobody handled. Failures report
`Net R: n/a` — never `+0.00`, which a sweep would average in as a real trade.";

/// `replay-candles` command-line arguments. Shared between the standalone
/// binary and `tv-arm ... replay`.
#[derive(Parser, Debug)]
#[command(name = "replay-candles")]
#[command(version = env!("GIT_VERSION"))]
#[command(about = "Replay a candle window through the engine's decision logic, offline")]
#[command(after_long_help = EXIT_CODE_HELP)]
pub struct ReplayArgs {
    /// Path to the TradePlan JSON written by `tv-arm ... plan-out`. Required for a
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
    ///
    /// Repeatable, last one wins — see `--annotate`.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, overrides_with = "simulate")]
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
    ///
    /// Repeatable, last one wins. `tv-arm … replay` injects `--annotate true`
    /// as a default, so an operator passthrough (`replay -- --annotate false`)
    /// has to be able to override it — without `overrides_with`, `ArgAction::Set`
    /// rejects the second occurrence outright and there is no way to run a
    /// chained replay without drawing on the chart.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, overrides_with = "annotate")]
    pub annotate: bool,

    /// Also annotate *not-taken* trades — pending orders that never filled and
    /// entries the worker declined — as muted grey brackets at the fire bar. Only
    /// meaningful with `--annotate` (and implies it). Off by default, so a
    /// plain `--annotate` shows just the taken positions.
    ///
    /// Repeatable, last one wins — see `--annotate`.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set, overrides_with = "annotate_unfilled")]
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

    /// Which entry rule the plan was armed with, recorded into the saved fixture's
    /// `meta.json` as the grid's **column** axis: `normal` / `skip-bcr` /
    /// `strategy-v2` (any other value is stored verbatim). Passed through by
    /// `tv-arm … replay --save`, which knows its own flags; a fixture saved
    /// without it reads as `normal`.
    #[arg(long, value_name = "RULE", requires = "save")]
    pub arm_entry_rule: Option<String>,

    /// Record that the plan was armed with `--skip-calendar-bars` (news windows
    /// suppressed) — the grid's **row** axis.
    ///
    /// Must be passed explicitly because it is **not inferable** from the saved
    /// plan: a plan with no pause rules could equally mean "the calendar ran and
    /// found no events in the window" or "the calendar was skipped".
    #[arg(long, requires = "save")]
    pub arm_skip_calendar_bars: bool,

    /// Record that the plan was armed with `--skip-golden`.
    #[arg(long, requires = "save")]
    pub arm_skip_golden: bool,

    /// The `--start` cursor as typed at arm time, stored verbatim so the exact
    /// spelling round-trips for a later re-arm.
    #[arg(long, value_name = "TS", requires = "save")]
    pub arm_start: Option<String>,

    /// The **broker-qualified** TradingView symbol the geometry was read from,
    /// e.g. `TRADENATION:EURUSD`. Record it qualified: a bare `EURUSD` silently
    /// resolves to the OANDA feed, so an unqualified capture can be off the wrong
    /// price data and produce a plausible-but-wrong number.
    #[arg(long, value_name = "SYMBOL", requires = "save")]
    pub arm_chart_symbol: Option<String>,

    /// `git describe` of the tv-arm build that armed the plan. (The engine version
    /// is stamped automatically from this binary's own build.)
    #[arg(long, value_name = "VERSION", requires = "save")]
    pub arm_tv_arm_version: Option<String>,

    /// A pointer back to the journal page this fixture documents, e.g.
    /// `trade-124`. Makes the corpus cross-referenceable with the journal in both
    /// directions — and "which pages still lack a fixture?" answerable.
    #[arg(long, value_name = "REF", requires = "save")]
    pub trade_ref: Option<String>,

    /// Replay a saved fixture **offline**: load plan + candles + meta from
    /// `<fixtures-dir>/<--fixture>/` instead of pulling from the broker (no
    /// network, no env vars, no TradingView). Requires `--fixture`.
    #[arg(long, requires = "fixture")]
    pub test_mode: bool,

    /// Name of the fixture under `<fixtures-dir>/` to replay with `--test-mode`.
    #[arg(long, value_name = "NAME")]
    pub fixture: Option<String>,

    /// Replay **every** fixture whose directory name matches this glob (`*` and
    /// `?`), instead of the single `--fixture`. Turns grid generation into a pure
    /// offline transform: `--test-mode --fixtures-glob 'trade-124-*' --json`.
    ///
    /// A failing fixture is **recorded and the batch continues** — one bad fixture
    /// can't hide the rest. Combines with `--check` (gate the whole set) and
    /// `--rebless` (re-bless the whole set).
    #[arg(
        long,
        value_name = "GLOB",
        requires = "test_mode",
        conflicts_with = "fixture"
    )]
    pub fixtures_glob: Option<String>,

    /// Emit the result as JSON on stdout instead of the human report.
    ///
    /// **Always emits an object, even when the replay fails** — a failure is a row
    /// with `ok: false`, an `error`, and a null `outcome`, never a missing row. So
    /// absence of output can only mean the process died unhandled, and a
    /// legitimately flat run (`ok: true`, `net_r: 0.0`) is never confused with a
    /// crash. That distinction is why this exists: scraping `Net R:` off stdout
    /// couldn't make it, and a concurrent batch silently lost cells because of it.
    ///
    /// **`--test-mode` only**, and enforced by clap rather than trusted. The
    /// guarantee above is a property of the fixture path, which owns a row schema
    /// (`BatchResult`) and can therefore describe its own failure. The live
    /// `--plan` path has no such schema: it emitted *zero bytes* under `--json` on
    /// failure (the terminal `FailureLine` is suppressed to keep stdout pure, and
    /// nothing replaced it), and the human report on success — the exact ambiguity
    /// this flag exists to remove, on the path an operator would use to *build* a
    /// corpus. A `requires` is the honest fix: better to refuse the flag than to
    /// invent a second, half-specified schema a driver would have to sniff.
    #[arg(long, requires = "test_mode")]
    pub json: bool,

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

    /// Score this batch against a blessed baseline file and report what moved:
    /// aggregate Net R, which fixtures changed, and by how much.
    ///
    /// This is **tier 2** — the scored corpus, not the pass/fail gate. It does
    /// *not* affect the exit code on its own: a moved number is information, not
    /// a failure. At 291 trades a legitimate engine fix moves hundreds of cells,
    /// and if a bug fix moves nothing it either didn't matter or isn't fixed. Use
    /// `--check` when you want a gate.
    ///
    /// Batch only — a one-fixture diff is just the fixture's own number.
    #[arg(long, value_name = "FILE", requires = "fixtures_glob")]
    pub baseline: Option<PathBuf>,

    /// Write this batch's results to a baseline file, to be diffed against later
    /// with `--baseline`.
    ///
    /// Only **successful** fixtures are blessed. A failed one is simply absent —
    /// we don't know what it earns, and recording a `0.0` for it would bake an
    /// infrastructure blip into the corpus as a real flat trade.
    ///
    /// Overwrites without asking, like `--rebless`. Keep baselines in git; the
    /// review of a re-bless is the diff.
    #[arg(long, value_name = "FILE", requires = "fixtures_glob")]
    pub bless_baseline: Option<PathBuf>,

    /// Label recorded in a blessed baseline (e.g. `v113`), shown in later diffs
    /// as the "from" side. Defaults to the engine version the fixtures carry.
    #[arg(long, value_name = "LABEL", requires = "bless_baseline")]
    pub baseline_label: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(d: DirectionFilter, g: GoldenFilter) -> DetectorMarkConfig {
        DetectorMarkConfig::new(d, g, Direction::Long)
    }

    /// `tv-arm … replay` injects `--annotate true` ahead of the operator's
    /// passthrough tokens, so a later `--annotate false` must win rather than
    /// being rejected as a duplicate. Without `overrides_with` clap errors with
    /// "cannot be used multiple times" and there is no way to run a chained
    /// replay without drawing on the chart — bad for unattended batches.
    #[test]
    fn repeated_bool_flags_take_the_last_value() {
        use clap::Parser as _;

        let args = ReplayArgs::try_parse_from([
            "replay-candles",
            "--annotate",
            "true",
            "--simulate",
            "true",
            "--annotate-unfilled",
            "true",
            // the operator's passthrough, appended last:
            "--annotate",
            "false",
            "--simulate",
            "false",
            "--annotate-unfilled",
            "false",
        ])
        .expect("repeated bool flags must parse, last one winning");

        assert!(!args.annotate, "last --annotate should win");
        assert!(!args.simulate, "last --simulate should win");
        assert!(
            !args.annotate_unfilled,
            "last --annotate-unfilled should win"
        );
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
