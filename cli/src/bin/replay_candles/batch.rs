//! Run many fixtures offline and emit machine-readable results.
//!
//! This is what turns the fixture corpus from a regression gate into an analysis
//! substrate. `--test-mode --fixtures-glob 'trade-124-*' --json` replays every
//! matching fixture and prints one JSON object per fixture: its name, how it was
//! armed, and what it earned. Building a 6-cell entry-sensitivity grid becomes a
//! pure offline transform — no TradingView, no broker, no network.
//!
//! ## Why `--json` always emits an object, even on failure
//!
//! The pre-existing way to get Net R out of a replay was to regex-scrape the
//! summary line from stdout. That is ambiguous in a way that silently corrupts
//! batch results: when a run **dies**, the summary line is simply *absent* — and
//! a legitimate no-fill replay also produces no meaningful R. From a driver's
//! perspective both are "no `Net R:`", so a crashed run and a real 0R result are
//! indistinguishable.
//!
//! That is not hypothetical. Running two batches concurrently against the
//! ReDB-backed candle cache (which takes an exclusive file lock), each batch
//! silently lost a *different random subset* of its cells. Both grids looked
//! complete and plausible. It was caught only because a cell that had previously
//! been `+0.52` came back empty.
//!
//! So: **every fixture in a `--json` run produces exactly one object**, and each
//! object carries an explicit `ok` flag. A failure is a row with `ok: false`, an
//! `error` string, and a null `outcome` — never a missing row. Absence of a row
//! can then only ever mean "the process died in a way nobody handled", which is a
//! condition a driver can actually detect.
//!
//! A batch also **keeps going** after a failing fixture (recording the failure)
//! rather than aborting the run, so one bad fixture can't hide the other 290.
//!
//! ## Parallel-safe by construction
//!
//! `--test-mode` reads frozen candles from disk and makes **no candle-cache
//! calls at all**, so a batch here is immune to the exclusive-lock contention
//! that affects the live-arm path. Verified: six concurrent `--test-mode` replays
//! of the same fixture all return identical results, none dropped.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::arm_record::ArmRecord;
use super::economics::ReplayEconomics;

/// One fixture's batch result. Serialized as one JSON object per fixture.
///
/// `ok` is the load-bearing field: it separates "this fixture replayed and here
/// is its economic result" from "this fixture could not be replayed". Never infer
/// either from the presence of `outcome` alone — a legitimately flat run has an
/// outcome with `net_r: 0.0`, which is a *result*, not a failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchResult {
    /// Fixture directory name, e.g. `trade-124-skip-bcr-newson`.
    pub fixture: String,
    /// Did the replay complete? `false` means `error` is set and `outcome` is
    /// `None` — the fixture was not scored and must not be counted.
    pub ok: bool,
    /// Why it failed. `None` when `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The golden this run **disagreed with**, when the failure was a `--check`
    /// mismatch rather than an inability to replay.
    ///
    /// A mismatch is a different animal from a broken fixture: the replay ran
    /// fine and produced a perfectly good `outcome` — we just don't agree with
    /// what was blessed. So the row keeps its `outcome` (see the field below) and
    /// records the expected Net R here, which is what makes a red sweep
    /// *readable*: "cell moved 0.35 → -0.62" instead of "cell failed". Without
    /// it, the only place the numbers survived was inside a multi-KB error string
    /// holding both pretty-printed JSON blobs — 291 regressed cells came to
    /// ~3.3 MB of duplicated diffs with zero machine-readable values.
    ///
    /// `None` for every other failure (and for a pass).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_net_r: Option<f64>,
    /// How the fixture was armed — the grid axes. `None` for a fixture saved
    /// before the `arm` block existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm: Option<ArmRecord>,
    /// The grid cell this fixture belongs to (`skip-bcr/news-off`), derived from
    /// `arm` so a consumer doesn't have to recompute it. `None` without an `arm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<String>,
    /// The trade this fixture is a variant of, from the frozen plan. Together
    /// with `cell` this is the full grid coordinate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trade_id: Option<String>,
    /// What it earned.
    ///
    /// `None` when the fixture couldn't be replayed at all, or when the run
    /// wasn't simulated. **Present on a `--check` mismatch** — see
    /// [`Self::mismatched`]: that run scored fine, so throwing its number away
    /// would be discarding the very thing you need to judge the regression.
    ///
    /// So `outcome.is_some()` does NOT imply `ok`. Filter on `ok` for the
    /// aggregate; read `outcome` on a `!ok` row to see what it measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ReplayEconomics>,
}

impl BatchResult {
    /// A successful replay.
    pub fn ok(
        fixture: &str,
        trade_id: Option<String>,
        arm: Option<ArmRecord>,
        outcome: Option<ReplayEconomics>,
    ) -> Self {
        Self {
            fixture: fixture.to_string(),
            ok: true,
            error: None,
            expected_net_r: None,
            cell: arm.as_ref().map(|a| a.cell_key()),
            arm,
            trade_id,
            outcome,
        }
    }

    /// A fixture that could not be replayed. Recorded as a row, never dropped.
    ///
    /// For a `--check` mismatch use [`Self::mismatched`] instead — that one ran,
    /// and its numbers are worth keeping.
    pub fn failed(fixture: &str, error: impl std::fmt::Display) -> Self {
        Self {
            fixture: fixture.to_string(),
            ok: false,
            error: Some(error.to_string()),
            expected_net_r: None,
            arm: None,
            cell: None,
            trade_id: None,
            outcome: None,
        }
    }

    /// A fixture that replayed cleanly but **disagreed with its golden**.
    ///
    /// Takes the `ok` row this run produced and demotes it: `ok: false` (so it
    /// stays out of the aggregate and the exit code still fails) while keeping
    /// every measurement — `outcome`, `arm`, `cell`, `trade_id` — plus the
    /// golden's Net R for comparison.
    ///
    /// This is the difference between a red sweep you can read and one you can't.
    /// A driver can compute `outcome.net_r - expected_net_r` per cell and see
    /// which regressions are noise and which are real, without parsing a
    /// human-formatted diff out of an error string.
    pub fn mismatched(
        ok_row: Self,
        expected: Option<&ReplayEconomics>,
        error: impl std::fmt::Display,
    ) -> Self {
        Self {
            ok: false,
            error: Some(error.to_string()),
            expected_net_r: expected.map(|e| e.net_r),
            ..ok_row
        }
    }

    /// Net R, or `0.0` when there's nothing scored. Convenience for aggregation —
    /// note a **failed** row also reads 0.0, so filter on `ok` first.
    pub fn net_r(&self) -> f64 {
        self.outcome.as_ref().map(|o| o.net_r).unwrap_or(0.0)
    }
}

/// The result of a whole batch: every fixture's row plus a roll-up.
///
/// `failed` is surfaced at the top level so a driver can check one field instead
/// of scanning rows — a non-zero `failed` means the aggregate below it is
/// incomplete and must not be treated as an answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchSummary {
    /// How many fixtures matched the glob.
    pub matched: usize,
    /// How many replayed successfully.
    pub succeeded: usize,
    /// How many failed. **Non-zero invalidates `net_r`** — see the struct doc.
    pub failed: usize,
    /// Sum of `net_r` across the successful rows only.
    pub net_r: f64,
    pub results: Vec<BatchResult>,
}

impl BatchSummary {
    pub fn from_results(results: Vec<BatchResult>) -> Self {
        let succeeded = results.iter().filter(|r| r.ok).count();
        let net_r: f64 = results.iter().filter(|r| r.ok).map(|r| r.net_r()).sum();
        Self {
            matched: results.len(),
            succeeded,
            failed: results.len() - succeeded,
            // An empty sum is `-0.0` on some paths, which formats as `-0.00` and
            // reads like a tiny real loss. Normalise it away.
            net_r: net_r + 0.0,
            results,
        }
    }

    /// A one-line human summary for the non-`--json` path.
    pub fn headline(&self) -> String {
        let mut line = format!(
            "batch: {} fixture(s) — {} ok, {} failed  |  Net R: {:+.2}",
            self.matched,
            self.succeeded,
            self.failed,
            // `{:+.2}` renders a negative zero as `-0.00`; force it positive so a
            // flat batch reads as flat.
            if self.net_r == 0.0 { 0.0 } else { self.net_r },
        );
        if self.failed > 0 {
            // Say it plainly: a partial batch is not an answer.
            line.push_str("  ← INCOMPLETE, net R excludes the failures");
        }
        line
    }
}

/// Match a fixture name against a shell-style glob supporting `*` (any run of
/// characters, including none) and `?` (exactly one character).
///
/// Deliberately hand-rolled rather than pulling a glob crate: the corpus naming
/// convention only ever needs prefix/suffix/infix wildcards (`trade-124-*`,
/// `*-news-off`), and this keeps the dependency surface of a money-path binary
/// unchanged. Anything more exotic belongs in a shell pipeline.
pub fn glob_matches(pattern: &str, name: &str) -> bool {
    // Walk both strings, remembering the last `*` so we can backtrack.
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut retry) = (None, 0usize);
    while ni < n.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some(pi);
                pi += 1;
                retry = ni;
            }
            Some('?') => {
                pi += 1;
                ni += 1;
            }
            Some(c) if *c == n[ni] => {
                pi += 1;
                ni += 1;
            }
            // Mismatch: backtrack to just after the last `*`, consuming one more
            // character with it. No `*` to fall back on → no match.
            _ => match star {
                Some(s) => {
                    pi = s + 1;
                    retry += 1;
                    ni = retry;
                }
                None => return false,
            },
        }
    }
    // Trailing `*`s in the pattern may still match the empty remainder.
    p[pi..].iter().all(|c| *c == '*')
}

/// The fixture directories under `root` whose names match `pattern`, sorted for
/// deterministic output (so two runs of the same batch produce diffable JSON).
///
/// A missing root yields an empty list rather than an error — the caller reports
/// "no fixtures matched", which is more useful than a path error.
pub fn matching_fixtures(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| glob_matches(pattern, n))
        })
        .collect();
    dirs.sort();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_candles::arm_record::EntryRule;

    #[test]
    fn glob_exact_and_star() {
        assert!(glob_matches("trade-124", "trade-124"));
        assert!(!glob_matches("trade-124", "trade-125"));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("trade-124-*", "trade-124-skip-bcr"));
        assert!(!glob_matches("trade-124-*", "trade-125-skip-bcr"));
        // A trailing `*` matches the empty remainder.
        assert!(glob_matches("trade-124*", "trade-124"));
    }

    #[test]
    fn glob_suffix_infix_and_question() {
        assert!(glob_matches("*-news-off", "trade-124-normal-news-off"));
        assert!(!glob_matches("*-news-off", "trade-124-normal-news-on"));
        assert!(glob_matches("trade-*-news-on", "trade-99-normal-news-on"));
        assert!(glob_matches("trade-12?", "trade-124"));
        assert!(!glob_matches("trade-12?", "trade-1240"));
    }

    /// Backtracking: a `*` that first matches too little must retry. Naive
    /// left-to-right matching gets this wrong.
    #[test]
    fn glob_backtracks_over_repeated_literals() {
        assert!(glob_matches("*-news-on", "t-news-on-news-on"));
        assert!(glob_matches("*abc", "zzabcabc"));
        assert!(!glob_matches("*abc", "zzabcx"));
    }

    #[test]
    fn multiple_stars() {
        assert!(glob_matches("*normal*", "trade-124-normal-news-on"));
        assert!(glob_matches(
            "trade-*-*-news-on",
            "trade-124-normal-news-on"
        ));
        assert!(!glob_matches("*normal*", "trade-124-skip-bcr-news-on"));
    }

    fn arm(rule: EntryRule, skip_calendar_bars: bool) -> ArmRecord {
        ArmRecord {
            entry_rule: rule,
            skip_calendar_bars,
            ..Default::default()
        }
    }

    fn econ(net_r: f64) -> ReplayEconomics {
        ReplayEconomics {
            net_r,
            ..ReplayEconomics::new()
        }
    }

    /// A successful row derives its grid cell from the arm block, so a consumer
    /// gets the coordinate without recomputing it.
    #[test]
    fn ok_row_carries_its_grid_cell() {
        let r = BatchResult::ok(
            "trade-124-skip-bcr-news-off",
            Some("hs-eurusd-abc".into()),
            Some(arm(EntryRule::SkipBcr, true)),
            Some(econ(-0.48)),
        );
        assert!(r.ok);
        assert_eq!(r.cell.as_deref(), Some("skip-bcr/news-off"));
        assert_eq!(r.trade_id.as_deref(), Some("hs-eurusd-abc"));
        assert!((r.net_r() + 0.48).abs() < 1e-9);
    }

    /// The contract that kills the ambiguity: a failure is a ROW, with `ok:
    /// false` and an error — not a missing row and not a silent 0R.
    #[test]
    fn failed_row_is_explicit_not_absent() {
        let r = BatchResult::failed("trade-77-normal-news-on", "cache lock");
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("cache lock"));
        assert!(r.outcome.is_none());
        // It reads 0.0 — which is exactly why a consumer must filter on `ok`
        // rather than treating 0.0 as "flat".
        assert_eq!(r.net_r(), 0.0);
    }

    /// A `--check` mismatch keeps every measurement it made, and records the
    /// golden's Net R beside it.
    ///
    /// This is what makes a red sweep readable: 291 regressed cells that each say
    /// "measured 0.35, expected -0.62" are diagnosable, whereas 291 rows that say
    /// only "failed" (with the numbers buried in an 11 KB pretty-printed diff)
    /// tell you *that* something moved and nothing about *what to*.
    #[test]
    fn a_check_mismatch_keeps_its_measurements_and_the_golden_it_missed() {
        let ok_row = BatchResult::ok(
            "trade-124-normal-news-on",
            Some("hs-eurusd-abc".into()),
            Some(arm(EntryRule::Normal, false)),
            Some(econ(0.35)),
        );
        let r = BatchResult::mismatched(ok_row, Some(&econ(-0.62)), "golden mismatch: <big diff>");

        // Still a failure: out of the aggregate, and the exit code fails.
        assert!(!r.ok);
        assert!(r.error.is_some());
        // …but nothing measured was thrown away.
        assert_eq!(r.outcome.as_ref().map(|o| o.net_r), Some(0.35));
        assert_eq!(r.expected_net_r, Some(-0.62));
        assert_eq!(r.cell.as_deref(), Some("normal/news-on"));
        assert_eq!(r.trade_id.as_deref(), Some("hs-eurusd-abc"));
        // The delta a driver actually wants is now computable from the row.
        let delta =
            r.outcome.as_ref().map(|o| o.net_r).unwrap_or(0.0) - r.expected_net_r.unwrap_or(0.0);
        assert!((delta - 0.97).abs() < 1e-9, "delta was {delta}");
    }

    /// `outcome.is_some()` must NOT be read as "passed" — a mismatch row has one.
    /// The aggregate has to filter on `ok`, and this pins that a mismatch really
    /// does stay out of it.
    #[test]
    fn a_mismatch_row_has_an_outcome_but_is_excluded_from_the_aggregate() {
        let ok_row = BatchResult::ok("b", None, None, Some(econ(0.35)));
        let s = BatchSummary::from_results(vec![
            BatchResult::ok("a", None, None, Some(econ(0.52))),
            BatchResult::mismatched(ok_row, Some(&econ(-0.62)), "mismatch"),
        ]);
        assert_eq!(s.succeeded, 1);
        assert_eq!(s.failed, 1);
        assert!(
            (s.net_r - 0.52).abs() < 1e-9,
            "the mismatched row's 0.35 must not be summed in: {}",
            s.net_r
        );
    }

    /// A non-replay failure has no measurements to keep, so it carries neither an
    /// outcome nor an expected value — `expected_net_r` is specifically the
    /// "disagreed with the golden" signal, not a generic field.
    #[test]
    fn a_broken_fixture_carries_no_expected_net_r() {
        let r = BatchResult::failed("t", "cache lock");
        assert!(r.outcome.is_none());
        assert!(r.expected_net_r.is_none());
    }

    /// A legitimately flat run is a RESULT, not a failure. This is the distinction
    /// stdout-scraping could not make.
    #[test]
    fn a_flat_run_is_ok_and_distinguishable_from_a_failure() {
        let flat = BatchResult::ok("t", None, None, Some(econ(0.0)));
        let died = BatchResult::failed("t", "boom");
        assert_eq!(flat.net_r(), died.net_r(), "both read 0.0 …");
        assert_ne!(flat.ok, died.ok, "… but `ok` tells them apart");
        assert!(flat.outcome.is_some() && died.outcome.is_none());
    }

    /// The roll-up sums only successful rows, and flags that it's incomplete.
    #[test]
    fn summary_excludes_failures_and_says_so() {
        let s = BatchSummary::from_results(vec![
            BatchResult::ok("a", None, None, Some(econ(0.52))),
            BatchResult::ok("b", None, None, Some(econ(-0.48))),
            BatchResult::failed("c", "cache lock"),
        ]);
        assert_eq!((s.matched, s.succeeded, s.failed), (3, 2, 1));
        assert!((s.net_r - 0.04).abs() < 1e-9, "net_r was {}", s.net_r);
        assert!(
            s.headline().contains("INCOMPLETE"),
            "a partial batch must not read as an answer: {}",
            s.headline()
        );
    }

    /// A clean batch doesn't cry wolf.
    #[test]
    fn clean_summary_has_no_incomplete_warning() {
        let s = BatchSummary::from_results(vec![BatchResult::ok("a", None, None, Some(econ(1.0)))]);
        assert_eq!(s.failed, 0);
        assert!(!s.headline().contains("INCOMPLETE"));
        assert!(s.headline().contains("Net R: +1.00"));
    }

    /// An empty batch is a valid, honest zero — not an error.
    #[test]
    fn empty_batch_summarises_cleanly() {
        let s = BatchSummary::from_results(Vec::new());
        assert_eq!((s.matched, s.succeeded, s.failed), (0, 0, 0));
        assert_eq!(s.net_r, 0.0);
    }

    /// A flat batch must print `+0.00`, never `-0.00` — a negative zero reads like
    /// a tiny real loss and would be alarming in a grid cell.
    #[test]
    fn flat_batch_never_prints_negative_zero() {
        for results in [
            Vec::new(),
            vec![BatchResult::ok("a", None, None, Some(econ(0.0)))],
            // A pair that cancels exactly.
            vec![
                BatchResult::ok("a", None, None, Some(econ(1.0))),
                BatchResult::ok("b", None, None, Some(econ(-1.0))),
            ],
        ] {
            let line = BatchSummary::from_results(results).headline();
            assert!(
                !line.contains("-0.00"),
                "flat batch must not read as a loss: {line}"
            );
        }
    }

    /// `matching_fixtures` finds only directories whose names match, sorted — so
    /// two runs of the same batch emit diffable JSON.
    #[test]
    fn matching_fixtures_filters_and_sorts() {
        let root = std::env::temp_dir().join(format!("batch-glob-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("trade-2-normal")).unwrap();
        std::fs::create_dir_all(root.join("trade-1-normal")).unwrap();
        std::fs::create_dir_all(root.join("trade-1-skip-bcr")).unwrap();
        std::fs::create_dir_all(root.join("other-thing")).unwrap();
        // A stray FILE matching the glob must be ignored — only dirs are fixtures.
        std::fs::write(root.join("trade-3-normal"), b"not a fixture").unwrap();

        let names: Vec<String> = matching_fixtures(&root, "trade-1-*")
            .iter()
            .filter_map(|p| p.file_name()?.to_str().map(String::from))
            .collect();
        assert_eq!(names, vec!["trade-1-normal", "trade-1-skip-bcr"]);

        let all: Vec<String> = matching_fixtures(&root, "trade-*")
            .iter()
            .filter_map(|p| p.file_name()?.to_str().map(String::from))
            .collect();
        assert_eq!(
            all,
            vec!["trade-1-normal", "trade-1-skip-bcr", "trade-2-normal"],
            "sorted, dirs only (the stray file is excluded)"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A missing fixtures root is an empty batch, not an error — the caller reports
    /// "no fixtures matched", which is more actionable than a path error.
    #[test]
    fn missing_root_is_an_empty_batch() {
        let root = std::env::temp_dir().join("definitely-not-a-real-fixtures-dir-xyz");
        assert!(matching_fixtures(&root, "*").is_empty());
    }

    /// The 6-cell grid, assembled from batch rows exactly as a consumer would: one
    /// trade, six variants, grouped by cell. This is the end the whole feature
    /// exists to serve.
    #[test]
    fn six_rows_assemble_into_a_grid() {
        let rows: Vec<BatchResult> = [
            (EntryRule::Normal, false, 0.52),
            (EntryRule::Normal, true, 0.52),
            (EntryRule::SkipBcr, false, -0.48),
            (EntryRule::SkipBcr, true, -0.48),
            (EntryRule::StrategyV2, false, -0.48),
            (EntryRule::StrategyV2, true, -0.48),
        ]
        .into_iter()
        .map(|(rule, news_off, r)| {
            // Both axes in the directory name. This omitted `news_off` and so
            // built 6 rows from only 3 distinct names — the test passed anyway
            // (it groups by `cell`, which was correct), but it modelled a naming
            // convention that would have collided on disk. The name is what a
            // `--fixtures-glob` sweep iterates, so it has to be unique per cell.
            let news = if news_off { "news-off" } else { "news-on" };
            BatchResult::ok(
                &format!("trade-124-{}-{news}", rule.label()),
                Some("trade-124".into()),
                Some(arm(rule, news_off)),
                Some(econ(r)),
            )
        })
        .collect();

        // Six cells means six directories: a colliding convention would silently
        // overwrite fixtures at save time, long before this aggregation.
        let names: std::collections::HashSet<&str> =
            rows.iter().map(|r| r.fixture.as_str()).collect();
        assert_eq!(
            names.len(),
            6,
            "one directory per grid cell, got {}: {names:?}",
            names.len()
        );

        let summary = BatchSummary::from_results(rows);
        assert_eq!(summary.succeeded, 6);
        assert_eq!(summary.failed, 0);

        let cells: std::collections::HashMap<String, f64> = summary
            .results
            .iter()
            .filter(|r| r.ok)
            .filter_map(|r| Some((r.cell.clone()?, r.net_r())))
            .collect();
        assert_eq!(cells.len(), 6, "six distinct cells: {cells:?}");
        assert!((cells["normal/news-on"] - 0.52).abs() < 1e-9);
        assert!((cells["skip-bcr/news-off"] + 0.48).abs() < 1e-9);
        // The standing question the grid answers: does the BCR gate save R?
        // Here normal beats skip-bcr on both news rows.
        assert!(cells["normal/news-on"] > cells["skip-bcr/news-on"]);
    }

    /// The whole summary round-trips, so a driver can persist a batch and diff it
    /// against a later run (the tier-2 baseline workflow).
    #[test]
    fn summary_json_round_trips() {
        let s = BatchSummary::from_results(vec![
            BatchResult::ok(
                "a",
                Some("t1".into()),
                Some(arm(EntryRule::Normal, false)),
                Some(econ(0.52)),
            ),
            BatchResult::failed("b", "nope"),
        ]);
        let back: BatchSummary =
            serde_json::from_str(&serde_json::to_string_pretty(&s).unwrap()).unwrap();
        assert_eq!(s, back);
    }
}
