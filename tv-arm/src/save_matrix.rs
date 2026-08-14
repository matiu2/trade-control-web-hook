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
//!
//! ## The news axis needs TWO applications, not one
//!
//! Read-the-chart-once is what makes the grid trustworthy, and it is also what
//! broke the news axis for as long as the matrix has existed. The other two axes
//! (entry rule, SL anchor) act on flags read *downstream* of [`SetupInputs`], so
//! setting them in [`Variant::apply`] is enough. The news axis is different: its
//! flag is consumed **upstream**, by `resolve_control_windows`, which has already
//! run by the time the matrix loops. So `apply` set `skip_calendar_bars` on a
//! copy of the args that nothing would ever read again, and every `news-off` cell
//! armed the news rules anyway — same pause/resume/news-start/news-end rules as
//! its `news-on` twin, differing only in `trade_id`.
//!
//! That failure was invisible in the worst way: the cell *recorded*
//! `skip_calendar_bars: true` in its fixture metadata, so on disk it claimed to
//! be news-off while carrying a full set of news rules. Measured across the
//! corpus when it was found: **13 of 26 setups** had news rules present in a cell
//! named `news-off` (the other 13 had no calendar events in window, so both cells
//! were legitimately empty and the mislabel was undetectable).
//!
//! Hence [`Variant::apply_to_setup`]: the news axis is applied to the *setup*,
//! not just the args, by swapping in [`ControlWindows::empty`] — the same value
//! `resolve_control_windows` returns for `--skip-calendar-bars`, so a news-off
//! cell is identical to one armed with the flag on the command line.
//!
//! **Both must be called.** `apply` alone silently rearms news; `apply_to_setup`
//! alone leaves the recorded metadata claiming news was on. They are separate
//! because they have different inputs, and `arm_the_matrix` calls both.
//!
//! Suppressing rather than re-fetching is deliberate: a second `calendar_windows`
//! call per cell would re-read a calendar that may have moved between cells,
//! reintroducing exactly the "comparing setups rather than flags" problem the
//! single chart read exists to prevent.

use crate::args::Args;
use crate::control_windows::ControlWindows;
use crate::setup_inputs::SetupInputs;
use crate::sl_anchor::SlAnchor;

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
    /// Which level the stop-loss is anchored to (`--sl-anchor`).
    ///
    /// [`SlAnchor::Signal`] on every cell of the base [`GRID`], so the default
    /// 8-cell matrix stays byte-identical to before this axis existed. Only
    /// [`sl_grid`] varies it.
    pub sl_anchor: SlAnchor,
}

impl Variant {
    /// `<entry-rule>-<news-on|news-off>` — the fixture directory name.
    ///
    /// Both axes are in the name deliberately. A convention that omitted the
    /// news axis would give six cells only three directory names, and the later
    /// three saves would silently overwrite the first three — a half-empty grid
    /// that looks complete.
    ///
    /// The SL axis appends `-sl-<anchor>` **only** for a non-default anchor, so
    /// every pre-existing fixture directory name is unchanged. Adding a
    /// `-sl-signal` suffix to the default cells would rename all 206 of them and
    /// orphan the corpus for no gain.
    pub fn fixture_suffix(&self) -> String {
        let news = if self.skip_calendar_bars {
            "news-off"
        } else {
            "news-on"
        };
        let sl = if self.sl_anchor.is_structural() {
            format!("-{}", self.sl_anchor.label())
        } else {
            String::new()
        };
        format!("{}-{news}{sl}", self.entry_rule)
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
        args.sl_anchor = self.sl_anchor;
        // The one expansion `apply_aliases` performs for these flags.
        if self.skip_bcr {
            args.skip_break_and_close = true;
            args.skip_retest = true;
        }
        // A matrix run must never write a spec per cell: the whole point is that
        // all six share ONE frozen setup.
        args.spec_out = None;
        // NOTE: setting `skip_calendar_bars` above is necessary but NOT
        // sufficient — the calendar has already been resolved into the shared
        // `SetupInputs` by now, so this copy only reaches the recorded metadata.
        // `apply_to_setup` is what actually suppresses the windows. See the
        // module doc.
        // Suffix the replay's `--save <name>` so each cell lands in its OWN
        // fixture directory. Without this all six saves collide on one name and
        // the last cell silently overwrites the other five.
        if let Some(crate::args::Command::Replay { args: replay }) = args.command.as_mut() {
            suffix_save_name(replay, &self.fixture_suffix());
        }
        args
    }

    /// Apply this variant's **news** axis to the shared setup.
    ///
    /// The companion to [`Self::apply`], which handles the entry-rule and SL
    /// axes. This one exists because the news flag is consumed *upstream* of
    /// [`SetupInputs`]: by the time the matrix loops, `resolve_control_windows`
    /// has already turned the calendar into `setup.control`, and no later reader
    /// consults `args.skip_calendar_bars` again. Varying the flag alone therefore
    /// changed nothing except the cell's recorded metadata — see the module doc
    /// for how that presented on disk.
    ///
    /// Suppression, not re-fetch: [`ControlWindows::empty`] is exactly what
    /// `resolve_control_windows` returns under `--skip-calendar-bars`, so a
    /// news-off cell here is indistinguishable from one armed with the flag
    /// directly. Re-fetching per cell would re-read a calendar that may have
    /// moved mid-run, which is the failure the single chart read prevents.
    ///
    /// A news-**on** cell is returned untouched, so the common path clones
    /// nothing it doesn't have to.
    pub fn apply_to_setup(&self, setup: SetupInputs) -> SetupInputs {
        if !self.skip_calendar_bars {
            return setup;
        }
        SetupInputs {
            control: ControlWindows::empty(),
            ..setup
        }
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
        sl_anchor: SlAnchor::Signal,
    },
    Variant {
        entry_rule: "normal",
        skip_bcr: false,
        strategy_v2: false,
        qm_entry: None,
        skip_calendar_bars: true,
        sl_anchor: SlAnchor::Signal,
    },
    Variant {
        entry_rule: "skip-bcr",
        skip_bcr: true,
        strategy_v2: false,
        qm_entry: None,
        skip_calendar_bars: false,
        sl_anchor: SlAnchor::Signal,
    },
    Variant {
        entry_rule: "skip-bcr",
        skip_bcr: true,
        strategy_v2: false,
        qm_entry: None,
        skip_calendar_bars: true,
        sl_anchor: SlAnchor::Signal,
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
        sl_anchor: SlAnchor::Signal,
    },
    Variant {
        entry_rule: "strategy-v2",
        skip_bcr: false,
        strategy_v2: true,
        qm_entry: None,
        skip_calendar_bars: true,
        sl_anchor: SlAnchor::Signal,
    },
    Variant {
        entry_rule: "strategy-v2-qm-market",
        skip_bcr: false,
        strategy_v2: true,
        qm_entry: Some(crate::args::QmEntry::Market),
        skip_calendar_bars: false,
        sl_anchor: SlAnchor::Signal,
    },
    Variant {
        entry_rule: "strategy-v2-qm-market",
        skip_bcr: false,
        strategy_v2: true,
        qm_entry: Some(crate::args::QmEntry::Market),
        skip_calendar_bars: true,
        sl_anchor: SlAnchor::Signal,
    },
];

/// The stop-loss axis: the shipped default plus the two structural levels.
///
/// Ordered widest-last so a grid reads tight → structural.
pub const SL_AXIS: [SlAnchor; 3] = [SlAnchor::Signal, SlAnchor::Invalidation, SlAnchor::FibTop];

/// The grid to run: the base 8 cells, or all 24 when `--sl-matrix` is set.
///
/// ## Why this is opt-in
///
/// The SL axis triples the cell count, and the matrix loop is **sequential** —
/// each cell shells out to `replay-candles` (`crate::replay::run_replay`), so 24
/// cells is 3× the wall-clock of 8. Making that the default would slow every
/// existing corpus run to answer a question most of them aren't asking.
///
/// Leaving it off also keeps the default 8 cells producing byte-identical
/// fixture names, so the existing corpus stays valid rather than being orphaned
/// by a rename.
///
/// ## Why the product rather than a v2-only slice
///
/// The tight-stop claim interacts with entry precision: a tight stop survives
/// only if the entry is precise enough that noise doesn't clip it, which is
/// exactly what the v2 confirming candle buys. Crossing SL against *every* entry
/// rule is what makes that interaction visible; a v2-only slice would answer the
/// narrower question and leave the interesting one unanswered. An operator who
/// wants the narrow slice can still pass `--sl-anchor` with a plain arm.
pub fn grid_for(sl_matrix: bool) -> Vec<Variant> {
    if !sl_matrix {
        return GRID.to_vec();
    }
    SL_AXIS
        .iter()
        .flat_map(|&sl_anchor| GRID.iter().map(move |base| Variant { sl_anchor, ..*base }))
        .collect()
}

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
    use crate::control_windows::AsOf;
    use crate::news_marker::NewsMarker;
    use crate::news_window::NewsWindow;
    use chrono::{DateTime, Utc};
    use clap::Parser;
    use trade_control_cli::Impact;

    fn base() -> Args {
        Args::try_parse_from(["tv-arm"]).expect("parse")
    }

    /// A setup carrying real calendar windows, as a live arm would produce.
    ///
    /// `SetupInputs::tests::sample` deliberately carries `ControlWindows::empty`,
    /// which is the *result* the news-off path is supposed to produce — so it
    /// cannot distinguish "suppressed" from "never had any" and would pass
    /// against a `apply_to_setup` that did nothing at all.
    fn setup_with_news() -> SetupInputs {
        let at = |s: &str| {
            DateTime::parse_from_rfc3339(s)
                .expect("valid rfc3339")
                .with_timezone(&Utc)
        };
        // Far future, so `ControlWindows::new`'s elapsed-prune keeps them: a
        // past window would be dropped at construction and this fixture would
        // silently become the empty case it exists to avoid.
        let win = NewsWindow::new(at("2099-01-01T00:00:00Z"), at("2099-01-01T01:00:00Z"));
        let marker = NewsMarker::new("USD", Impact::High, at("2099-01-01T00:30:00Z"));
        let control = ControlWindows::new(
            vec![win],
            vec![win],
            vec![marker],
            AsOf::wallclock(at("2026-01-01T00:00:00Z")),
        );
        assert!(
            control.has_news() && !control.blackout().is_empty(),
            "fixture must actually carry windows, else the suppression tests prove nothing"
        );
        SetupInputs {
            control,
            ..crate::setup_inputs::tests::sample()
        }
    }

    fn variant_with_news(skip_calendar_bars: bool) -> Variant {
        Variant {
            skip_calendar_bars,
            ..GRID[0]
        }
    }

    /// The news-OFF cell must actually lose its windows.
    ///
    /// This is the regression that motivated `apply_to_setup`. Before it, a
    /// `news-off` cell armed the full set of pause/news rules and only its
    /// recorded metadata said otherwise — 13 of 26 corpus setups had news rules
    /// sitting in a directory named `news-off`.
    #[test]
    fn a_news_off_cell_suppresses_the_calendar_windows() {
        let out = variant_with_news(true).apply_to_setup(setup_with_news());
        assert!(
            !out.control.has_news(),
            "news-off must drop the news windows"
        );
        assert!(
            out.control.blackout().is_empty(),
            "news-off must drop the blackout (pause/resume) windows too — a pause \
             is calendar-derived and skipping the calendar must skip both"
        );
        assert!(
            out.control.markers().is_empty(),
            "news-off must drop the markers, keeping drawn == armed"
        );
    }

    /// The news-ON cell must keep them — otherwise the axis is dead in the other
    /// direction and every cell is news-off.
    #[test]
    fn a_news_on_cell_keeps_the_calendar_windows() {
        let out = variant_with_news(false).apply_to_setup(setup_with_news());
        assert!(out.control.has_news(), "news-on must keep the news windows");
        assert!(
            !out.control.blackout().is_empty(),
            "news-on must keep the blackout windows"
        );
    }

    /// Suppression touches the calendar and NOTHING else.
    ///
    /// The grid's whole premise is that cells differ only in the flag under
    /// test. If the news axis also perturbed geometry or resolution, the
    /// news-on/news-off comparison would be measuring two setups rather than one
    /// flag — the exact failure the single chart read exists to prevent.
    #[test]
    fn suppression_changes_only_the_control_windows() {
        let before = setup_with_news();
        let after = variant_with_news(true).apply_to_setup(before.clone());
        assert_eq!(after.geom, before.geom, "geometry must be untouched");
        assert_eq!(after.resolution, before.resolution);
        assert_eq!(after.chart_symbol, before.chart_symbol);
        assert_eq!(after.instrument, before.instrument);
        assert_eq!(after.start, before.start);
        // `AsOf` has no `PartialEq` and doesn't get one just to satisfy a test —
        // compare the instant it carries, which is the load-bearing part.
        assert_eq!(after.prune_as_of.at, before.prune_as_of.at);
    }

    /// Every news-off cell in the real grid suppresses, and every news-on keeps.
    ///
    /// Walks `GRID` rather than a hand-built variant so a newly-added cell is
    /// covered automatically — a new row that forgot the axis would otherwise
    /// only be caught by someone re-reading this file.
    #[test]
    fn every_grid_cell_matches_its_news_label() {
        for v in grid_for(true) {
            let out = v.apply_to_setup(setup_with_news());
            let suffix = v.fixture_suffix();
            if suffix.contains("news-off") {
                assert!(
                    !out.control.has_news() && out.control.blackout().is_empty(),
                    "{suffix} is named news-off but kept calendar windows"
                );
            } else {
                assert!(
                    out.control.has_news(),
                    "{suffix} is named news-on but lost its news windows"
                );
            }
        }
    }

    /// `apply` must keep setting the flag even though `apply_to_setup` does the
    /// real work: the flag is what lands in the fixture's `arm` metadata, and a
    /// cell whose data says news-off while its metadata says news-on is the same
    /// class of silent lie, just inverted.
    #[test]
    fn apply_still_records_the_news_flag_for_the_metadata() {
        assert!(variant_with_news(true).apply(&base()).skip_calendar_bars);
        assert!(!variant_with_news(false).apply(&base()).skip_calendar_bars);
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

    // ---- the stop-loss axis ----------------------------------------------

    /// Without `--sl-matrix` the grid is exactly the old 8 cells, every one on
    /// the default anchor. The SL axis must cost nothing when unused.
    #[test]
    fn the_sl_axis_is_off_by_default() {
        let grid = grid_for(false);
        assert_eq!(grid.len(), 8);
        assert!(grid.iter().all(|v| v.sl_anchor == SlAnchor::Signal));
    }

    /// **The corpus-compatibility guarantee.** Every fixture name the default
    /// grid produces must be byte-identical to the pre-feature name, or all 206
    /// existing fixture directories are orphaned by a rename.
    #[test]
    fn default_cells_keep_their_original_fixture_names() {
        let names: Vec<String> = grid_for(false).iter().map(|v| v.fixture_suffix()).collect();
        assert_eq!(
            names,
            vec![
                "normal-news-on",
                "normal-news-off",
                "skip-bcr-news-on",
                "skip-bcr-news-off",
                "strategy-v2-news-on",
                "strategy-v2-news-off",
                "strategy-v2-qm-market-news-on",
                "strategy-v2-qm-market-news-off",
            ],
            "a default cell's fixture name changed — this orphans the corpus"
        );
    }

    /// `--sl-matrix` is the full 3× product, and every cell still has a
    /// distinct directory name.
    #[test]
    fn the_sl_matrix_is_the_full_product_with_distinct_names() {
        let grid = grid_for(true);
        assert_eq!(grid.len(), 24, "3 anchors × 8 base cells");
        let names: std::collections::HashSet<String> =
            grid.iter().map(|v| v.fixture_suffix()).collect();
        assert_eq!(names.len(), 24, "colliding fixture names");
        // The base 8 keep their bare names; the structural cells are suffixed.
        assert!(names.contains("normal-news-on"), "{names:?}");
        assert!(
            names.contains("normal-news-on-sl-invalidation"),
            "{names:?}"
        );
        assert!(names.contains("normal-news-on-sl-fib-top"), "{names:?}");
    }

    /// Each anchor appears on every base cell — the axis is a true product, not
    /// a few cells sprinkled in.
    #[test]
    fn every_anchor_covers_every_base_cell() {
        let grid = grid_for(true);
        for anchor in SL_AXIS {
            assert_eq!(
                grid.iter().filter(|v| v.sl_anchor == anchor).count(),
                8,
                "{anchor:?} must cover all 8 base cells"
            );
        }
    }

    /// The SL axis varies ONLY the anchor: the entry-rule and news axes of each
    /// base cell survive untouched. A cell that silently changed its entry rule
    /// would be attributing an entry difference to the stop.
    #[test]
    fn the_sl_axis_varies_only_the_stop() {
        for (i, v) in grid_for(true).iter().enumerate() {
            let base = &GRID[i % 8];
            assert_eq!(v.entry_rule, base.entry_rule);
            assert_eq!(v.skip_bcr, base.skip_bcr);
            assert_eq!(v.strategy_v2, base.strategy_v2);
            assert_eq!(v.qm_entry, base.qm_entry);
            assert_eq!(v.skip_calendar_bars, base.skip_calendar_bars);
        }
    }

    /// `apply` puts the anchor onto the args, so the cell actually arms with the
    /// stop it names. Without this the whole axis is cosmetic — 24 directories
    /// holding 8 distinct results.
    #[test]
    fn apply_sets_the_sl_anchor_on_the_args() {
        let v = Variant {
            sl_anchor: SlAnchor::FibTop,
            ..GRID[0]
        };
        assert_eq!(v.apply(&base()).sl_anchor, SlAnchor::FibTop);
    }

    /// A structural cell's `--save` name is suffixed, so it lands in its own
    /// fixture directory rather than overwriting the default cell's.
    #[test]
    fn a_structural_cell_saves_to_its_own_directory() {
        let b = Args::try_parse_from(["tv-arm", "replay", "--save", "trade-7"]).expect("parse");
        let v = Variant {
            sl_anchor: SlAnchor::Invalidation,
            ..GRID[0]
        };
        assert_eq!(
            v.apply(&b).replay_args(),
            ["--save", "trade-7-normal-news-on-sl-invalidation"]
        );
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
