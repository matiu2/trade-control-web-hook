//! `--save-matrix`: one chart read, every entry-sensitivity variant.
//!
//! The standing question the corpus answers is whether the gates earn their
//! keep: does the break-and-close/retest prep chain net-save R, does the v2
//! confirming candle, does the news calendar. Answering it needs the **same
//! setup** armed several ways, so the only difference between the resulting
//! fixtures is the flag under test.
//!
//! Doing that by hand is six invocations of `tv-arm`, each re-reading the
//! chart. That is slow at 291 trades, and — worse — **not actually the same
//! setup**: every read re-runs role classification against a chart that may have
//! scrolled, and re-reads a calendar that may have moved. Six reads can
//! legitimately produce six slightly different `SetupInputs`, and then the grid
//! is comparing setups rather than flags.
//!
//! So the matrix reads the chart **once** and re-arms from that single
//! [`SetupInputs`], varying only the flags. Every cell is guaranteed to share
//! byte-identical geometry.
//!
//! ## The axes
//!
//! Four entry rules × news on/off = eight cells:
//!
//! | | news-on | news-off |
//! |---|---|---|
//! | **normal** | full gate chain | " |
//! | **skip-bcr** | no preps | " |
//! | **strategy-v2** | QM **limit** leg + confirming candle | " |
//! | **strategy-v2-qm-market** | QM **market** leg + confirming candle | " |
//!
//! The last two differ only in the QM leg's order type, and they answer
//! different questions: the limit leg asks *"does waiting for the pullback pay
//! for the fills it misses?"*, the market leg *"is the confirmation candle
//! alone enough?"*. They are separate columns rather than one because folding
//! them together would average a fill-rate difference into a returns
//! difference and hide both.
//!
//! The cell names match [`EntryRule::label`] in the replay side's `arm_record`,
//! because a batch tool groups on exactly that string. Renaming one without the
//! other silently splits a grid column in two.
//!
//! ## Why a failing cell doesn't abort the matrix
//!
//! A variant can legitimately fail to arm — `--strategy-v2` needs a Quasimodo
//! leg the drawing may not support, and a validation gate can reject one entry
//! rule while accepting another. Aborting on the first would throw away the
//! cells that *did* work, and at 291 trades that's a slow way to learn nothing.
//! Each cell is recorded with its outcome and the run continues; the summary
//! says plainly how many armed.

use crate::args::Args;

/// One cell of the entry-sensitivity grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Variant {
    /// Entry-rule label. **Must** match `EntryRule::label` on the replay side —
    /// a batch tool groups grid columns on this exact string.
    pub entry_rule: &'static str,
    /// Skip the break-and-close + retest preps (`--skip-bcr`).
    pub skip_bcr: bool,
    /// Arm the Quasimodo limit leg with a confirming candle (`--strategy-v2`).
    pub strategy_v2: bool,
    /// Order type for the strategy-v2 QM leg (`--qm-entry`). `None` is the
    /// default (limit). Only meaningful when `strategy_v2` is set — the flag
    /// `requires = "strategy_v2"` at the clap layer.
    pub qm_entry: Option<crate::args::QmEntry>,
    /// Skip the news calendar entirely (`--skip-calendar-bars`).
    pub skip_calendar_bars: bool,
}

impl Variant {
    /// `<entry-rule>-<news-on|news-off>` — the fixture directory name.
    ///
    /// Both axes are in the name deliberately. A convention that omitted the
    /// news axis would give six cells only three directory names, and the later
    /// three saves would silently overwrite the first three — a half-empty grid
    /// that looks complete.
    pub fn fixture_suffix(&self) -> String {
        let news = if self.skip_calendar_bars {
            "news-off"
        } else {
            "news-on"
        };
        format!("{}-{news}", self.entry_rule)
    }

    /// Apply this variant's flags to a copy of the operator's args.
    ///
    /// Mirrors `Args::apply_aliases` **exactly**, because that has already run by
    /// the time the matrix loops — setting `skip_bcr` alone here would be a no-op
    /// and the cell would silently arm as `normal`, giving the grid two identical
    /// columns and a false conclusion about whether the preps earn their keep.
    ///
    /// Note what mirroring means for `--strategy-v2`: it does **not** expand to
    /// anything. `apply_aliases` expands `--skip-bcr` and `--quasimodo`, but
    /// `strategy_v2` is read directly downstream and adds the `09-enter-qm` leg
    /// *alongside* the preps. An earlier version of this function also set
    /// `skip_break_and_close` / `require_confirmation` for the v2 cell — the
    /// resulting plans happened to match by luck, but the flags didn't, and a
    /// grid column that doesn't mean what its name says is worse than a missing
    /// one. Verified against the real flag: both produce the same 8 rules,
    /// including `09-enter-qm`.
    pub fn apply(&self, base: &Args) -> Args
    where
        Args: Clone,
    {
        let mut args = base.clone();
        args.skip_bcr = self.skip_bcr;
        args.strategy_v2 = self.strategy_v2;
        args.qm_entry = self.qm_entry;
        args.skip_calendar_bars = self.skip_calendar_bars;
        // The one expansion `apply_aliases` performs for these flags.
        if self.skip_bcr {
            args.skip_break_and_close = true;
            args.skip_retest = true;
        }
        // A matrix run must never write a spec per cell: the whole point is that
        // all six share ONE frozen setup.
        args.spec_out = None;
        // Suffix the replay's `--save <name>` so each cell lands in its OWN
        // fixture directory. Without this all six saves collide on one name and
        // the last cell silently overwrites the other five.
        if let Some(crate::args::Command::Replay { args: replay }) = args.command.as_mut() {
            suffix_save_name(replay, &self.fixture_suffix());
        }
        args
    }
}

/// Rewrite `--save <name>` (and `--fixture <name>`) in a replay passthrough to
/// `<name>-<suffix>`.
///
/// Handles both `--save name` and `--save=name`. A passthrough with no `--save`
/// is left alone — the operator asked for a matrix of *arms*, not of saves, and
/// inventing a fixture name they didn't ask for would litter the corpus.
fn suffix_save_name(argv: &mut [String], suffix: &str) {
    const FLAGS: [&str; 2] = ["--save", "--fixture"];
    let mut i = 0;
    while i < argv.len() {
        // `--save=<name>`
        if let Some((flag, name)) = argv[i].split_once('=')
            && FLAGS.contains(&flag)
        {
            argv[i] = format!("{flag}={name}-{suffix}");
        // `--save <name>` — only when a value actually follows, and it isn't
        // itself a flag (`--save --json` would mean the operator forgot the
        // name; rewriting `--json` into a fixture name would be worse than
        // leaving `replay-candles` to report the missing value).
        } else if FLAGS.contains(&argv[i].as_str())
            && let Some(next) = argv.get(i + 1)
            && !next.starts_with('-')
        {
            argv[i + 1] = format!("{next}-{suffix}");
            i += 1;
        }
        i += 1;
    }
}

/// The eight cells, in a stable order so two matrix runs are diffable.
pub const GRID: [Variant; 8] = [
    Variant {
        entry_rule: "normal",
        skip_bcr: false,
        strategy_v2: false,
        qm_entry: None,
        skip_calendar_bars: false,
    },
    Variant {
        entry_rule: "normal",
        skip_bcr: false,
        strategy_v2: false,
        qm_entry: None,
        skip_calendar_bars: true,
    },
    Variant {
        entry_rule: "skip-bcr",
        skip_bcr: true,
        strategy_v2: false,
        qm_entry: None,
        skip_calendar_bars: false,
    },
    Variant {
        entry_rule: "skip-bcr",
        skip_bcr: true,
        strategy_v2: false,
        qm_entry: None,
        skip_calendar_bars: true,
    },
    Variant {
        entry_rule: "strategy-v2",
        skip_bcr: false,
        strategy_v2: true,
        // `None`, not `Some(Limit)`: limit IS the default, and leaving the flag
        // off keeps this cell byte-identical to typing `--strategy-v2` alone —
        // which is what every fixture captured before `--qm-entry` existed froze.
        qm_entry: None,
        skip_calendar_bars: false,
    },
    Variant {
        entry_rule: "strategy-v2",
        skip_bcr: false,
        strategy_v2: true,
        qm_entry: None,
        skip_calendar_bars: true,
    },
    Variant {
        entry_rule: "strategy-v2-qm-market",
        skip_bcr: false,
        strategy_v2: true,
        qm_entry: Some(crate::args::QmEntry::Market),
        skip_calendar_bars: false,
    },
    Variant {
        entry_rule: "strategy-v2-qm-market",
        skip_bcr: false,
        strategy_v2: true,
        qm_entry: Some(crate::args::QmEntry::Market),
        skip_calendar_bars: true,
    },
];

/// How one cell turned out.
#[derive(Debug)]
pub struct CellOutcome {
    pub variant: Variant,
    /// Exit code from `arm_from_inputs`, or the error if it failed outright.
    pub result: Result<i32, String>,
}

impl CellOutcome {
    /// Did this cell arm cleanly? A non-zero exit is an operator-facing
    /// rejection (a gate said no), which is a **result**, not a crash.
    pub fn armed(&self) -> bool {
        matches!(self.result, Ok(0))
    }
}

/// Human summary of a matrix run.
///
/// Says how many cells armed out of how many, and names the ones that didn't.
/// Silence about a missing cell is how a half-empty grid gets mistaken for a
/// complete one.
pub fn summarise(outcomes: &[CellOutcome]) -> String {
    let armed = outcomes.iter().filter(|o| o.armed()).count();
    let mut out = format!("save-matrix: {armed}/{} cell(s) armed", outcomes.len());
    for o in outcomes.iter().filter(|o| !o.armed()) {
        let why = match &o.result {
            Ok(code) => format!("rejected (exit {code})"),
            Err(e) => e.lines().next().unwrap_or("failed").to_string(),
        };
        out.push_str(&format!("\n  ✗ {:<24} {why}", o.variant.fixture_suffix()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn base() -> Args {
        Args::try_parse_from(["tv-arm"]).expect("parse")
    }

    /// Eight cells, eight DISTINCT directory names.
    ///
    /// A convention that omitted the news axis would map eight cells onto four
    /// names, and the later saves would overwrite the earlier ones — half the
    /// grid gone, and it would still look complete.
    #[test]
    fn every_cell_has_a_distinct_fixture_name() {
        let names: std::collections::HashSet<String> =
            GRID.iter().map(|v| v.fixture_suffix()).collect();
        assert_eq!(names.len(), 8, "colliding fixture names: {names:?}");
        assert!(names.contains("normal-news-on"));
        assert!(names.contains("skip-bcr-news-off"));
        assert!(names.contains("strategy-v2-news-on"));
        assert!(names.contains("strategy-v2-qm-market-news-on"));
        assert!(names.contains("strategy-v2-qm-market-news-off"));
    }

    /// The grid is exactly the 4×2 product — no duplicates, nothing missing.
    #[test]
    fn the_grid_is_the_full_product_of_both_axes() {
        let mut seen: Vec<(&str, bool)> = GRID
            .iter()
            .map(|v| (v.entry_rule, v.skip_calendar_bars))
            .collect();
        seen.sort_unstable();
        assert_eq!(
            seen,
            vec![
                ("normal", false),
                ("normal", true),
                ("skip-bcr", false),
                ("skip-bcr", true),
                ("strategy-v2", false),
                ("strategy-v2", true),
                ("strategy-v2-qm-market", false),
                ("strategy-v2-qm-market", true),
            ]
        );
    }

    /// Entry-rule labels must match the replay side's `EntryRule::label`, which
    /// is what a batch tool groups grid columns on. A rename on one side alone
    /// splits a column in two and nothing errors.
    #[test]
    fn entry_rule_labels_match_the_replay_side() {
        let labels: Vec<&str> = GRID
            .iter()
            .map(|v| v.entry_rule)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(
            labels,
            vec!["normal", "skip-bcr", "strategy-v2", "strategy-v2-qm-market"]
        );
    }

    /// `apply` sets the UNDERLYING flags, not just the alias.
    ///
    /// `apply_aliases` has already run by the time the matrix loops, so setting
    /// `skip_bcr` alone would leave `skip_break_and_close` / `skip_retest` off
    /// and the cell would arm as `normal` — two identical grid columns, no
    /// error, and a false conclusion about whether the preps earn their keep.
    #[test]
    fn apply_expands_the_alias_rather_than_relying_on_apply_aliases() {
        let v = GRID
            .iter()
            .find(|v| v.entry_rule == "skip-bcr")
            .expect("skip-bcr cell");
        let args = v.apply(&base());
        assert!(args.skip_bcr);
        assert!(
            args.skip_break_and_close && args.skip_retest,
            "the alias must be expanded here — apply_aliases already ran"
        );
    }

    /// `strategy-v2` sets ONLY `strategy_v2`, matching what the real flag does.
    ///
    /// `apply_aliases` expands `--skip-bcr` and `--quasimodo`; it leaves
    /// `strategy_v2` alone, and the pipeline reads it directly to add the
    /// `09-enter-qm` leg *alongside* the preps. An earlier version of `apply`
    /// also set `skip_break_and_close` / `require_confirmation` here. The plans
    /// still matched — by luck — but the flags didn't, and a grid column that
    /// doesn't mean what its name says is worse than a missing one.
    #[test]
    fn strategy_v2_mirrors_the_real_flag_and_does_not_skip_the_preps() {
        let v = GRID
            .iter()
            .find(|v| v.entry_rule == "strategy-v2")
            .expect("v2 cell");
        let args = v.apply(&base());
        assert!(args.strategy_v2);
        assert!(!args.skip_bcr, "v2 is its own column, not skip-bcr");
        assert!(
            !args.skip_break_and_close && !args.skip_retest,
            "v2 adds the QM leg ALONGSIDE the preps — it does not skip them"
        );
        assert!(
            !args.require_confirmation,
            "--strategy-v2 does not imply --require-confirmation; only --quasimodo does"
        );

        // The load-bearing property: `apply` must agree with `apply_aliases` on
        // the same input, so a matrix cell is byte-identical to typing the flag.
        let typed = Args::try_parse_from(["tv-arm", "--strategy-v2"])
            .expect("parse")
            .apply_aliases();
        assert_eq!(args.strategy_v2, typed.strategy_v2);
        assert_eq!(args.skip_break_and_close, typed.skip_break_and_close);
        assert_eq!(args.skip_retest, typed.skip_retest);
        assert_eq!(args.require_confirmation, typed.require_confirmation);
    }

    /// The QM-market cell must be identical to typing
    /// `--strategy-v2 --qm-entry market` by hand.
    ///
    /// The pairing is load-bearing: `--qm-entry` `requires = "strategy_v2"`, so a
    /// cell that set `qm_entry` without `strategy_v2` describes a flag
    /// combination clap would reject — it would never be caught here (the matrix
    /// mutates a parsed `Args` rather than re-parsing) and would arm as something
    /// no operator can type.
    #[test]
    fn qm_market_cell_matches_typing_the_flags() {
        let v = GRID
            .iter()
            .find(|v| v.entry_rule == "strategy-v2-qm-market")
            .expect("qm-market cell");
        let args = v.apply(&base());
        let typed = Args::try_parse_from(["tv-arm", "--strategy-v2", "--qm-entry", "market"])
            .expect("the flag pair must be legal to type")
            .apply_aliases();

        assert_eq!(args.strategy_v2, typed.strategy_v2);
        assert_eq!(args.qm_entry, typed.qm_entry);
        assert_eq!(args.qm_entry, Some(crate::args::QmEntry::Market));
        assert_eq!(args.skip_break_and_close, typed.skip_break_and_close);
        assert_eq!(args.skip_retest, typed.skip_retest);
        assert_eq!(args.require_confirmation, typed.require_confirmation);
    }

    /// The default-v2 cells must leave `--qm-entry` UNSET, not set it to `limit`.
    ///
    /// Limit is already the default, so both spell the same behaviour — but only
    /// the unset form is byte-identical to the `--strategy-v2` arms captured
    /// before `--qm-entry` existed. Setting it explicitly would make every old
    /// fixture non-reproducible for no gain.
    #[test]
    fn the_plain_v2_cells_leave_qm_entry_unset() {
        for v in GRID.iter().filter(|v| v.entry_rule == "strategy-v2") {
            assert_eq!(
                v.apply(&base()).qm_entry,
                None,
                "plain strategy-v2 must not pin --qm-entry"
            );
        }
    }

    /// Every cell that sets `qm_entry` must also set `strategy_v2` — the clap
    /// `requires` relationship the matrix bypasses by mutating `Args` directly.
    #[test]
    fn no_cell_sets_qm_entry_without_strategy_v2() {
        for v in GRID.iter().filter(|v| v.qm_entry.is_some()) {
            assert!(
                v.strategy_v2,
                "{} sets --qm-entry without --strategy-v2, which clap forbids",
                v.entry_rule
            );
        }
    }

    /// The same agreement check for `skip-bcr` — the cell must be identical to
    /// typing `--skip-bcr` by hand.
    #[test]
    fn skip_bcr_cell_matches_typing_the_flag() {
        let v = GRID
            .iter()
            .find(|v| v.entry_rule == "skip-bcr")
            .expect("skip-bcr cell");
        let args = v.apply(&base());
        let typed = Args::try_parse_from(["tv-arm", "--skip-bcr"])
            .expect("parse")
            .apply_aliases();
        assert_eq!(args.skip_bcr, typed.skip_bcr);
        assert_eq!(args.skip_break_and_close, typed.skip_break_and_close);
        assert_eq!(args.skip_retest, typed.skip_retest);
        assert_eq!(args.require_confirmation, typed.require_confirmation);
    }

    /// The `normal` cell leaves every gate on — it's the control.
    #[test]
    fn the_normal_cell_changes_nothing() {
        let v = &GRID[0];
        let args = v.apply(&base());
        assert!(!args.skip_bcr && !args.strategy_v2);
        assert!(!args.skip_break_and_close && !args.skip_retest);
        assert!(!args.skip_calendar_bars, "GRID[0] is normal/news-ON");
    }

    /// A matrix run never writes a spec per cell — all six share one frozen
    /// setup, which is the point.
    #[test]
    fn apply_clears_spec_out() {
        let mut b = base();
        b.spec_out = Some("/tmp/s.json".into());
        assert!(GRID[0].apply(&b).spec_out.is_none());
    }

    /// Each cell's `--save` name gets its own suffix, so six cells land in six
    /// directories.
    ///
    /// Without this every cell saves to the SAME name and the last one silently
    /// overwrites the other five — leaving one fixture where the grid expects
    /// six, with nothing reporting a problem.
    #[test]
    fn each_cell_saves_to_its_own_fixture_directory() {
        let base = Args::try_parse_from([
            "tv-arm",
            "replay",
            "--save",
            "trade-124",
            "--simulate",
            "true",
        ])
        .expect("parse");

        let names: Vec<String> = GRID
            .iter()
            .map(|v| {
                let args = v.apply(&base);
                let argv = args.replay_args().to_vec();
                let i = argv.iter().position(|a| a == "--save").expect("--save");
                argv[i + 1].clone()
            })
            .collect();

        assert_eq!(
            names.iter().collect::<std::collections::HashSet<_>>().len(),
            8,
            "eight cells must produce eight distinct fixture names: {names:?}"
        );
        assert!(
            names.contains(&"trade-124-normal-news-on".to_string()),
            "{names:?}"
        );
        assert!(
            names.contains(&"trade-124-strategy-v2-news-off".to_string()),
            "{names:?}"
        );
        assert!(
            names.contains(&"trade-124-strategy-v2-qm-market-news-on".to_string()),
            "{names:?}"
        );
    }

    /// `--save=<name>` (equals form) is suffixed too.
    #[test]
    fn the_equals_form_of_save_is_also_suffixed() {
        let base = Args::try_parse_from(["tv-arm", "replay", "--save=trade-9"]).expect("parse");
        let args = GRID[3].apply(&base);
        assert_eq!(
            args.replay_args(),
            ["--save=trade-9-skip-bcr-news-off"],
            "the equals form must be rewritten as well"
        );
    }

    /// A passthrough with no `--save` is left alone. The operator asked for a
    /// matrix of arms, not of saves; inventing a fixture name would litter the
    /// corpus with directories nobody asked for.
    #[test]
    fn a_replay_without_save_is_untouched() {
        let base = Args::try_parse_from(["tv-arm", "replay", "--simulate", "true"]).expect("parse");
        assert_eq!(GRID[0].apply(&base).replay_args(), ["--simulate", "true"]);
    }

    /// `--save` with no value (or followed by another flag) is left for
    /// `replay-candles` to reject. Rewriting the NEXT FLAG into a fixture name
    /// would turn a clear "missing value" error into a bizarre one.
    #[test]
    fn a_valueless_save_is_not_rewritten() {
        let mut argv = vec!["--save".to_string(), "--json".to_string()];
        suffix_save_name(&mut argv, "normal-news-on");
        assert_eq!(argv, ["--save", "--json"], "must not rewrite a flag");

        let mut trailing = vec!["--json".to_string(), "--save".to_string()];
        suffix_save_name(&mut trailing, "normal-news-on");
        assert_eq!(trailing, ["--json", "--save"], "must not run off the end");
    }

    /// Only the save/fixture name is touched — other values that happen to look
    /// similar are left alone.
    #[test]
    fn suffixing_touches_only_the_save_and_fixture_names() {
        let mut argv: Vec<String> = ["--source", "oanda", "--save", "t1", "--message", "save"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        suffix_save_name(&mut argv, "normal-news-on");
        assert_eq!(
            argv,
            [
                "--source",
                "oanda",
                "--save",
                "t1-normal-news-on",
                "--message",
                "save"
            ],
            "a VALUE that reads like the flag must not be rewritten"
        );
    }

    /// The operator's other flags survive — the matrix varies the grid axes and
    /// nothing else.
    #[test]
    fn apply_preserves_unrelated_flags() {
        let mut b = base();
        b.risk_amount = Some(5.0);
        b.skip_golden = true;
        let args = GRID[2].apply(&b);
        assert_eq!(args.risk_amount, Some(5.0));
        assert!(args.skip_golden);
    }

    fn cell(i: usize, result: Result<i32, String>) -> CellOutcome {
        CellOutcome {
            variant: GRID[i],
            result,
        }
    }

    /// A non-zero exit is a *rejection* (a gate said no), not a crash — but it
    /// still didn't arm, so it must not be counted as a cell.
    #[test]
    fn a_rejected_cell_is_not_armed() {
        assert!(cell(0, Ok(0)).armed());
        assert!(!cell(0, Ok(1)).armed());
        assert!(!cell(0, Err("boom".into())).armed());
    }

    /// The summary NAMES the cells that didn't arm. A count alone would let a
    /// half-empty grid read as complete.
    #[test]
    fn the_summary_names_every_missing_cell() {
        let outcomes = vec![
            cell(0, Ok(0)),
            cell(2, Ok(1)),
            cell(4, Err("no quasimodo leg\nbacktrace…".into())),
        ];
        let s = summarise(&outcomes);
        assert!(s.contains("1/3 cell(s) armed"), "{s}");
        assert!(s.contains("skip-bcr-news-on"), "{s}");
        assert!(s.contains("rejected (exit 1)"), "{s}");
        assert!(s.contains("strategy-v2-news-on"), "{s}");
        // Only the first line of a multi-line error, so a backtrace doesn't
        // bury the summary.
        assert!(s.contains("no quasimodo leg"), "{s}");
        assert!(!s.contains("backtrace"), "{s}");
    }

    /// A clean sweep says so without listing anything.
    #[test]
    fn a_full_sweep_lists_no_failures() {
        let outcomes: Vec<CellOutcome> = (0..GRID.len()).map(|i| cell(i, Ok(0))).collect();
        let s = summarise(&outcomes);
        assert_eq!(s, "save-matrix: 8/8 cell(s) armed");
    }
}
