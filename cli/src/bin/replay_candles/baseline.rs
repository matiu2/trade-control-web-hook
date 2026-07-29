//! Score a whole corpus against a blessed baseline, and say what moved.
//!
//! Tier 1 (`replay-fixtures/`, in `cargo test`) is a **gate**: ~20 hand-picked
//! fixtures, exact-equality, any change is a failure. Tier 2 — this module — is
//! the opposite animal: all 291 trades, **not** a pass/fail test, deliberately
//! not in `cargo test`.
//!
//! ## Why a second tier exists at all
//!
//! At 291 trades a legitimate engine fix breaks hundreds of `assert_eq!`
//! goldens. The tempting conclusion — "then never change existing behaviour" —
//! is wrong: most recent engine work (break-and-close zone-straddle, retest
//! slope-scaled tolerance, QM/v2 confirmation) *should* move R on historical
//! setups. **If a bug fix moves nothing, it either didn't matter or it isn't
//! fixed.**
//!
//! The real problem with a flat exact-equality corpus is that it yields **one
//! bit** — "300 failed" — when what's needed is *which way, by how much, and
//! which handful got worse*. So tier 2 scores and diffs rather than asserting.
//!
//! ## Structural moves outrank magnitude moves
//!
//! The diff deliberately does **not** rank purely by |ΔR|. A trade that flips
//! between *taken* and *not taken* is a different kind of event from one whose R
//! shifted: the engine changed its mind about whether there was a trade at all,
//! and that is worth reading even when the R delta is small. A not-taken trade
//! books exactly 0.0 R (see [`ReplayEconomics`]), so a flip to not-taken from
//! +0.05R would sort near the bottom of a magnitude-ranked list and never be
//! looked at — while a flip to not-taken from a *losing* trade shows up as an
//! improvement, which is exactly the kind of "profit" nobody should bank
//! without noticing.
//!
//! Hence [`MoveKind`]: `Entered`/`Stopped` are surfaced as their own bucket, and
//! only genuine magnitude changes get sorted by size.
//!
//! ## No noise threshold, on purpose
//!
//! There is no "changes under 0.05R are noise" cutoff. Replay is deterministic —
//! same fixture, same code, same number — so a non-zero delta is never noise, it
//! is always the code. A threshold would be a way of *not looking* at small
//! moves, and small moves across 291 trades are how a regression hides. The diff
//! sorts by magnitude so the operator can stop reading when they want to; it
//! doesn't decide for them.
//!
//! ## News-ON rows are advisory, not reproducible
//!
//! `close_on_news` derives from the calendar **re-read at arm time**, so a
//! news-ON cell can move because the calendar moved, not because the engine did.
//! Those rows are flagged [`Movement::calendar_sensitive`] so calendar drift is
//! never mistaken for a code regression.
//!
//! The alternative — freezing the calendar into each fixture — was rejected:
//! it would make news-ON rows answer a question about a *stale* calendar, which
//! is not the question. Fresh news is correct behaviour; the diff just has to be
//! honest about which rows inherit it. News-OFF rows stay fully reproducible and
//! carry the load for regression detection.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::batch::{BatchResult, BatchSummary};
use super::economics::ReplayEconomics;

/// One fixture's blessed result: what it earned, and the code that earned it.
///
/// Stores `net_r` plus the counts that distinguish *how* the R was earned — a
/// trade that goes from "TP hit" to "reversal close" for the same net R has
/// changed materially even though the headline number didn't move.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineEntry {
    /// Net R at bless time.
    pub net_r: f64,
    /// Grid cell (`skip-bcr/news-off`), carried so a diff can group without
    /// re-reading every fixture's `meta.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<String>,
    /// The trade this fixture is a variant of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trade_id: Option<String>,
    /// Was this variant armed with the news calendar active? Drives the
    /// calendar-sensitivity flag on any movement — see the module doc.
    ///
    /// Stored rather than parsed back out of `cell` so a renamed cell convention
    /// can't silently turn a news-ON row into a reproducible one.
    #[serde(default)]
    pub news_on: bool,
    /// Did this variant take a trade at all? `false` means every fire was
    /// declined/never-filled and the fixture booked a structural 0.0.
    ///
    /// **Not** derivable from `net_r == 0.0`: a real trade can close at exactly
    /// break-even, and an open-at-end position books 0.0 too. Conflating those
    /// with "no trade" is precisely the mistake [`MoveKind`] exists to avoid.
    #[serde(default)]
    pub taken: bool,
    pub tp_hits: usize,
    pub sl_hits: usize,
    pub reversal_closes: usize,
    pub expiry_closes: usize,
    /// Flattened by the structure-invalidation `close-positions` veto. Separate
    /// from `expiry_closes` so a setup-broke close and a clock-ran-out close are
    /// a detectable change of character, not the same number.
    #[serde(default)]
    pub invalidation_closes: usize,
}

impl BaselineEntry {
    /// Build from a successful batch row. `None` for a row that didn't score —
    /// a failure has no result to bless, and blessing a `0.0` for it would bake
    /// an infrastructure blip into the corpus as a real flat trade.
    pub fn from_row(row: &BatchResult) -> Option<Self> {
        if !row.ok {
            return None;
        }
        let outcome = row.outcome.as_ref()?;
        Some(Self {
            net_r: outcome.net_r,
            cell: row.cell.clone(),
            trade_id: row.trade_id.clone(),
            news_on: row.arm.as_ref().is_some_and(|a| !a.skip_calendar_bars),
            taken: took_a_trade(outcome),
            tp_hits: outcome.tp_hits,
            sl_hits: outcome.sl_hits,
            reversal_closes: outcome.reversal_closes,
            expiry_closes: outcome.expiry_closes,
            invalidation_closes: outcome.invalidation_closes,
        })
    }

    /// The exit-shape counters, for detecting a same-R change of character.
    fn shape(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.tp_hits,
            self.sl_hits,
            self.reversal_closes,
            self.expiry_closes,
            self.invalidation_closes,
        )
    }
}

/// Did this outcome actually put a position on?
///
/// Reads the **legs**, not `net_r`. A fixture with no legs never traded; one
/// with legs traded even if they netted exactly zero.
fn took_a_trade(outcome: &ReplayEconomics) -> bool {
    !outcome.legs.is_empty()
}

/// A blessed corpus: every fixture's result plus the code version that produced
/// it.
///
/// The version fields are what make the baseline *interpretable* a month later.
/// A blessed baseline is the triple `(corpus, engine_version, aggregate)` — a
/// bare aggregate with no version stamp silently mixes pre- and post-fix numbers
/// and means nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    /// Free-text label for this bless, e.g. `v113`. Operator-supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The engine version that produced these numbers, taken from the fixtures
    /// themselves (not from the running binary) so a baseline blessed from old
    /// fixtures reports honestly.
    ///
    /// `None` when the corpus disagrees with itself — see [`common_version`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
    /// Aggregate net R across every blessed entry. Stored so a reader sees the
    /// headline without summing 291 rows — and so a hand-edited file that no
    /// longer adds up is detectable.
    pub net_r: f64,
    /// Fixture name → its blessed result. A `BTreeMap` so the serialized file is
    /// key-sorted and two blesses of the same corpus produce a minimal git diff.
    pub entries: BTreeMap<String, BaselineEntry>,
}

impl Baseline {
    /// Bless a batch. Only successful rows are recorded.
    ///
    /// A batch with failures **can** still be blessed — the caller decides — but
    /// the failed fixtures are simply absent, which a later diff reports as
    /// `added` when they come back. That's the honest representation: we don't
    /// know what they earn.
    pub fn from_summary(summary: &BatchSummary, label: Option<String>) -> Self {
        let entries: BTreeMap<String, BaselineEntry> = summary
            .results
            .iter()
            .filter_map(|r| Some((r.fixture.clone(), BaselineEntry::from_row(r)?)))
            .collect();
        // Sum in key order (BTreeMap iteration), NOT batch order. Float addition
        // isn't associative, so the order has to be one a re-read can reproduce:
        // a `Baseline` read back from disk iterates its BTreeMap, and if the
        // stored total had been summed in batch order the two could differ in the
        // last bits. Key order is the same on both sides.
        let net_r: f64 = entries.values().map(|e| e.net_r).sum();
        Self {
            label,
            engine_version: common_version(summary),
            // `-0.0` formats as `-0.00` and reads like a tiny loss.
            net_r: net_r + 0.0,
            entries,
        }
    }

    /// Recompute the aggregate from the entries.
    ///
    /// Exists so a hand-edited baseline (someone deletes a row to silence a
    /// regression) can be caught by [`Self::is_self_consistent`] rather than
    /// silently shifting the reported total.
    pub fn recomputed_net_r(&self) -> f64 {
        self.entries.values().map(|e| e.net_r).sum::<f64>() + 0.0
    }

    /// Does the stored aggregate match the entries?
    ///
    /// Exact comparison: [`Self::from_summary`] sums in the same key order this
    /// does, so a file we wrote always agrees bit-for-bit. A mismatch means the
    /// file was edited by something other than a bless.
    pub fn is_self_consistent(&self) -> bool {
        self.recomputed_net_r() == self.net_r
    }
}

/// The engine version shared by every scored row, or `None` if they disagree
/// (or nothing recorded one).
///
/// Disagreement is reported as absence rather than "whichever came first",
/// because a corpus captured across an engine change is exactly the situation
/// where a confidently-wrong single version does the most damage: it makes a
/// mixed baseline look coherent.
fn common_version(summary: &BatchSummary) -> Option<String> {
    let mut found: Option<&str> = None;
    for row in summary.results.iter().filter(|r| r.ok) {
        let v = row.arm.as_ref()?.engine_version.as_deref()?;
        match found {
            None => found = Some(v),
            Some(seen) if seen == v => {}
            Some(_) => return None,
        }
    }
    found.map(String::from)
}

/// What kind of change a fixture underwent. Ordered worst-to-read-first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MoveKind {
    /// The engine now takes a trade it previously declined.
    Entered,
    /// The engine now declines a trade it previously took. **Reads as an
    /// improvement whenever the old trade lost money** — which is why it is its
    /// own kind rather than a small negative delta.
    Stopped,
    /// Traded in both, and the R changed.
    Requantified,
    /// Traded in both for the same R, but the *exit shape* changed (a TP became
    /// a reversal close, say). Same headline, different behaviour.
    Reshaped,
    /// In the new run, absent from the baseline.
    Added,
    /// In the baseline, absent from the new run. Usually a fixture that failed
    /// to replay — check the batch rows before reading it as a deletion.
    Removed,
}

impl MoveKind {
    /// Structural moves (the engine changed its mind about whether to trade) are
    /// listed before magnitude moves regardless of |ΔR|.
    pub fn is_structural(&self) -> bool {
        matches!(
            self,
            Self::Entered | Self::Stopped | Self::Added | Self::Removed
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Entered => "entered",
            Self::Stopped => "stopped",
            Self::Requantified => "requantified",
            Self::Reshaped => "reshaped",
            Self::Added => "added",
            Self::Removed => "removed",
        }
    }
}

/// One fixture that changed between baseline and run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Movement {
    pub fixture: String,
    pub kind: MoveKind,
    /// Baseline net R. `0.0` for an [`MoveKind::Added`] fixture — read `kind`,
    /// not this, to tell "was flat" from "wasn't there".
    pub was: f64,
    /// New net R. `0.0` for [`MoveKind::Removed`].
    pub now: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trade_id: Option<String>,
    /// This row was armed with the news calendar live, so it can move for
    /// **calendar** reasons rather than engine reasons. Not a regression signal
    /// on its own — see the module doc.
    pub calendar_sensitive: bool,
}

impl Movement {
    pub fn delta(&self) -> f64 {
        self.now - self.was
    }
}

/// The result of diffing a run against a baseline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineDiff {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_engine_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
    /// Aggregate net R: baseline → run.
    pub was: f64,
    pub now: f64,
    /// Fixtures scored in both runs and unchanged.
    pub unchanged: usize,
    /// Every fixture that changed, structural moves first, then magnitude moves
    /// sorted by |ΔR| descending.
    pub moved: Vec<Movement>,
    /// Fixtures in the run that did **not** score (`ok: false`). Non-zero means
    /// the comparison is incomplete — the aggregate below excludes them, and
    /// they show as `removed` in `moved`.
    pub unscored: usize,
}

impl BaselineDiff {
    pub fn delta(&self) -> f64 {
        self.now - self.was
    }

    pub fn improved(&self) -> usize {
        self.moved.iter().filter(|m| m.delta() > 0.0).count()
    }

    pub fn worsened(&self) -> usize {
        self.moved.iter().filter(|m| m.delta() < 0.0).count()
    }

    /// Movements that can only be explained by the code — the calendar can't
    /// have caused them. This is the set a regression hunt should start from.
    pub fn reproducible(&self) -> impl Iterator<Item = &Movement> {
        self.moved.iter().filter(|m| !m.calendar_sensitive)
    }

    /// Did anything change at all?
    pub fn is_clean(&self) -> bool {
        self.moved.is_empty()
    }
}

/// Diff a fresh batch against a blessed baseline.
///
/// Only `ok` rows are compared. A row that failed to replay is counted in
/// `unscored` and shows up as [`MoveKind::Removed`] — deliberately not silently
/// skipped, because "this fixture stopped producing a number" is exactly the
/// kind of thing a batch driver must not mistake for "unchanged".
pub fn diff(baseline: &Baseline, summary: &BatchSummary) -> BaselineDiff {
    let fresh: BTreeMap<String, BaselineEntry> = summary
        .results
        .iter()
        .filter_map(|r| Some((r.fixture.clone(), BaselineEntry::from_row(r)?)))
        .collect();

    let mut moved = Vec::new();
    let mut unchanged = 0usize;

    for (name, old) in &baseline.entries {
        match fresh.get(name) {
            Some(new) => match classify(old, new) {
                Some(kind) => moved.push(movement(name, kind, old.net_r, new.net_r, new)),
                None => unchanged += 1,
            },
            None => moved.push(movement(name, MoveKind::Removed, old.net_r, 0.0, old)),
        }
    }
    for (name, new) in &fresh {
        if !baseline.entries.contains_key(name) {
            moved.push(movement(name, MoveKind::Added, 0.0, new.net_r, new));
        }
    }

    sort_movements(&mut moved);

    BaselineDiff {
        baseline_label: baseline.label.clone(),
        baseline_engine_version: baseline.engine_version.clone(),
        engine_version: common_version(summary),
        was: baseline.net_r,
        // Sum the FRESH entries in key order, matching `Baseline::from_summary`,
        // so `diff` against a baseline blessed from this same run reports a zero
        // delta exactly rather than a last-bit residue.
        now: fresh.values().map(|e| e.net_r).sum::<f64>() + 0.0,
        unchanged,
        moved,
        unscored: summary.failed,
    }
}

/// How (if at all) one fixture changed. `None` when nothing moved.
fn classify(old: &BaselineEntry, new: &BaselineEntry) -> Option<MoveKind> {
    match (old.taken, new.taken) {
        (false, true) => Some(MoveKind::Entered),
        (true, false) => Some(MoveKind::Stopped),
        // Exact compare: replay is deterministic, so any difference is the code.
        // An epsilon here would be a way of not looking at small moves.
        _ if old.net_r != new.net_r => Some(MoveKind::Requantified),
        _ if old.shape() != new.shape() => Some(MoveKind::Reshaped),
        _ => None,
    }
}

fn movement(name: &str, kind: MoveKind, was: f64, now: f64, meta: &BaselineEntry) -> Movement {
    Movement {
        fixture: name.to_string(),
        kind,
        was,
        now,
        cell: meta.cell.clone(),
        trade_id: meta.trade_id.clone(),
        calendar_sensitive: meta.news_on,
    }
}

/// Structural moves first, then by |ΔR| descending, then by name.
///
/// The name tiebreak matters: without it, two fixtures with identical deltas
/// could order differently between runs and make a diff-of-diffs noisy.
fn sort_movements(moved: &mut [Movement]) {
    moved.sort_by(|a, b| {
        b.kind
            .is_structural()
            .cmp(&a.kind.is_structural())
            .then_with(|| {
                b.delta()
                    .abs()
                    .partial_cmp(&a.delta().abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.fixture.cmp(&b.fixture))
    });
}

/// Render a diff the way the scoping doc describes it.
pub fn render(diff: &BaselineDiff, limit: usize) -> String {
    let mut out = String::new();
    let from = diff.baseline_label.as_deref().unwrap_or("baseline");
    let to = diff.engine_version.as_deref().unwrap_or("this run");
    out.push_str(&format!("vs {from} → {to}\n"));
    out.push_str(&format!(
        "  net R:  {:+.2} → {:+.2}  ({:+.2})\n",
        diff.was,
        diff.now,
        diff.delta()
    ));
    out.push_str(&format!(
        "  moved:  {} fixture(s)  ({} improved, {} worse)\n",
        diff.moved.len(),
        diff.improved(),
        diff.worsened()
    ));
    out.push_str(&format!("  same:   {}\n", diff.unchanged));

    if diff.is_clean() {
        out.push_str("  nothing moved.\n");
    } else {
        // Split the movers by whether the calendar could explain them. A sweep
        // where every mover is calendar-sensitive is a materially different
        // report from one with reproducible movers, and the difference is easy to
        // miss when it's only a `[calendar]` tag on individual rows.
        let repro = diff.reproducible().count();
        out.push_str(&format!(
            "  of which: {repro} reproducible, {} calendar-sensitive\n",
            diff.moved.len() - repro
        ));
        if repro == 0 {
            out.push_str(
                "  ← every mover was armed with the live calendar; none of this is \
                 necessarily a code change\n",
            );
        }
    }

    if diff.unscored > 0 {
        // Loud, because a partial comparison that reads as an answer is the
        // failure mode this whole layer exists to prevent.
        out.push_str(&format!(
            "  ← INCOMPLETE: {} fixture(s) did not score; the net R above excludes them\n",
            diff.unscored
        ));
    }

    for m in diff.moved.iter().take(limit) {
        let flag = if m.calendar_sensitive {
            "  [calendar]"
        } else {
            ""
        };
        out.push_str(&format!(
            "    {:<12} {:<40} {:+.2} → {:+.2}{flag}\n",
            m.kind.label(),
            m.fixture,
            m.was,
            m.now
        ));
    }
    if diff.moved.len() > limit {
        out.push_str(&format!("    … and {} more\n", diff.moved.len() - limit));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_candles::arm_record::{ArmRecord, EntryRule};
    use crate::replay_candles::economics::{ExitReason, Leg};
    use chrono::{TimeZone, Utc};

    /// A leg that realized `r`. The prices are plausible but arbitrary — nothing
    /// in this module reads them, it reads *presence* (see [`took_a_trade`]) and
    /// `r`.
    fn leg(r: f64) -> Leg {
        Leg {
            entry_time: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            entry_price: 1.0,
            stop_loss: 0.9,
            take_profit: 1.2,
            exit_time: Some(Utc.timestamp_opt(1_700_003_600, 0).unwrap()),
            exit_price: Some(1.1),
            exit_reason: ExitReason::TookProfit,
            r,
        }
    }

    fn econ(net_r: f64, legs: Vec<Leg>) -> ReplayEconomics {
        ReplayEconomics {
            net_r,
            legs,
            ..ReplayEconomics::new()
        }
    }

    /// A traded fixture: one leg, so `taken` is true.
    fn traded(name: &str, net_r: f64, news_on: bool) -> BatchResult {
        BatchResult::ok(
            name,
            Some("t1".into()),
            Some(ArmRecord {
                entry_rule: EntryRule::Normal,
                skip_calendar_bars: !news_on,
                engine_version: Some("v113".into()),
                ..Default::default()
            }),
            Some(econ(net_r, vec![leg(net_r)])),
        )
    }

    /// A not-taken fixture: zero legs, net R 0.0.
    fn not_taken(name: &str) -> BatchResult {
        BatchResult::ok(
            name,
            Some("t1".into()),
            Some(ArmRecord {
                engine_version: Some("v113".into()),
                ..Default::default()
            }),
            Some(econ(0.0, Vec::new())),
        )
    }

    fn summary(rows: Vec<BatchResult>) -> BatchSummary {
        BatchSummary::from_results(rows)
    }

    /// The central distinction of this module: a not-taken trade and a
    /// break-even trade both read `net_r == 0.0`, and they are NOT the same
    /// event. `taken` reads the legs, so it separates them.
    ///
    /// Without this, a fixture flipping from "took a trade that closed flat" to
    /// "declined to trade" would classify as unchanged and never be seen.
    #[test]
    fn a_breakeven_trade_is_taken_but_a_declined_one_is_not() {
        let flat_trade = BaselineEntry::from_row(&BatchResult::ok(
            "a",
            None,
            None,
            Some(econ(0.0, vec![leg(0.0)])),
        ))
        .expect("scored");
        let declined = BaselineEntry::from_row(&not_taken("b")).expect("scored");

        assert_eq!(flat_trade.net_r, declined.net_r, "both read 0.0 R …");
        assert!(flat_trade.taken, "… but one put a position on");
        assert!(!declined.taken, "… and one never did");

        // And the diff must call that a structural move, not "unchanged".
        assert_eq!(classify(&flat_trade, &declined), Some(MoveKind::Stopped));
        assert_eq!(classify(&declined, &flat_trade), Some(MoveKind::Entered));
    }

    /// A position still open at the window end books a leg and 0.0 R, so it
    /// counts as **taken**. That is deliberate: the engine did put a position on,
    /// and a later run that declines the same setup has genuinely changed its
    /// mind — a `Stopped`, not an unchanged 0.0.
    ///
    /// This is the third distinct thing that reads `net_r == 0.0` (with
    /// break-even and declined), and the reason `taken` reads the legs.
    #[test]
    fn a_still_open_position_counts_as_taken() {
        let open_leg = Leg {
            exit_time: None,
            exit_price: None,
            exit_reason: ExitReason::OpenAtWindowEnd,
            r: 0.0,
            ..leg(0.0)
        };
        let still_open = BaselineEntry::from_row(&BatchResult::ok(
            "a",
            None,
            None,
            Some(econ(0.0, vec![open_leg])),
        ))
        .expect("scored");

        assert_eq!(still_open.net_r, 0.0);
        assert!(
            still_open.taken,
            "a position was opened, even if unresolved"
        );

        let declined = BaselineEntry::from_row(&not_taken("b")).expect("scored");
        assert_eq!(
            classify(&still_open, &declined),
            Some(MoveKind::Stopped),
            "declining a setup we previously opened is a structural move"
        );
    }

    /// A failed row has no result to bless. Blessing `0.0` for it would bake an
    /// infrastructure blip into the corpus as a real flat trade.
    #[test]
    fn a_failed_row_is_not_blessable() {
        assert!(BaselineEntry::from_row(&BatchResult::failed("a", "lock")).is_none());
        // A `--check` mismatch also carries an outcome but is `ok: false` — it
        // must not be blessed either, or a red sweep would bless its own
        // regression.
        let mismatch = BatchResult::mismatched(traded("a", 0.5, false), None, "mismatch");
        assert!(mismatch.outcome.is_some(), "it does carry a measurement");
        assert!(
            BaselineEntry::from_row(&mismatch).is_none(),
            "but it must not be blessed"
        );
    }

    /// A clean re-run of the corpus that produced the baseline reports nothing.
    #[test]
    fn blessing_then_diffing_the_same_run_is_clean() {
        let s = summary(vec![traded("a", 0.52, false), traded("b", -0.48, false)]);
        let base = Baseline::from_summary(&s, Some("v113".into()));
        let d = diff(&base, &s);

        assert!(d.is_clean(), "moved: {:?}", d.moved);
        assert_eq!(d.unchanged, 2);
        assert_eq!(d.delta(), 0.0, "an unchanged corpus has zero drift");
        assert_eq!(d.engine_version.as_deref(), Some("v113"));
    }

    /// The scoping doc's headline example: aggregate moved, some improved, some
    /// worse, and the worst are listed first.
    #[test]
    fn a_regression_is_ranked_by_magnitude() {
        let base = Baseline::from_summary(
            &summary(vec![
                traded("trade-071", -1.00, false),
                traded("trade-118", 1.18, false),
                traded("trade-200", 0.10, false),
            ]),
            Some("v113".into()),
        );
        let after = summary(vec![
            traded("trade-071", -2.00, false), // −1.00
            traded("trade-118", 0.05, false),  // −1.13
            traded("trade-200", 0.15, false),  // +0.05
        ]);
        let d = diff(&base, &after);

        assert_eq!(d.moved.len(), 3);
        assert_eq!(d.improved(), 1);
        assert_eq!(d.worsened(), 2);
        // Biggest |ΔR| first: 1.13 (118) then 1.00 (071) then 0.05 (200).
        let order: Vec<&str> = d.moved.iter().map(|m| m.fixture.as_str()).collect();
        assert_eq!(order, vec!["trade-118", "trade-071", "trade-200"]);
        assert!(
            d.moved.iter().all(|m| m.kind == MoveKind::Requantified),
            "all three traded in both runs"
        );
    }

    /// Structural moves come first even when their |ΔR| is tiny — the whole
    /// reason `MoveKind` exists.
    ///
    /// Mutation-check: removing the `is_structural` term from `sort_movements`
    /// puts `big-swing` first and reddens this.
    #[test]
    fn a_structural_move_outranks_a_bigger_magnitude_move() {
        let base = Baseline::from_summary(
            &summary(vec![
                traded("tiny-flip", 0.02, false),
                traded("big-swing", 3.0, false),
            ]),
            None,
        );
        let after = summary(vec![
            not_taken("tiny-flip"),           // Δ −0.02, but structural
            traded("big-swing", -3.0, false), // Δ −6.0, but merely magnitude
        ]);
        let d = diff(&base, &after);

        assert_eq!(d.moved[0].fixture, "tiny-flip");
        assert_eq!(d.moved[0].kind, MoveKind::Stopped);
        assert_eq!(d.moved[1].fixture, "big-swing");
    }

    /// A fixture that stops trading a LOSS reads as a Net R improvement. It is
    /// surfaced as `stopped`, not banked silently as a win — because "the engine
    /// stopped taking this trade" needs a human to agree it was right to.
    #[test]
    fn no_longer_taking_a_loser_is_reported_not_quietly_banked() {
        let base = Baseline::from_summary(&summary(vec![traded("loser", -1.0, false)]), None);
        let d = diff(&base, &summary(vec![not_taken("loser")]));

        assert_eq!(d.delta(), 1.0, "the aggregate genuinely improves");
        assert_eq!(d.moved[0].kind, MoveKind::Stopped, "but it's flagged");
        assert!(!d.is_clean());
    }

    /// Same net R, different exit shape. The headline number can't see this, so
    /// the counters are compared too.
    #[test]
    fn a_same_r_change_of_exit_shape_is_still_a_move() {
        let tp = BaselineEntry {
            tp_hits: 1,
            ..BaselineEntry::from_row(&traded("a", 1.0, false)).expect("scored")
        };
        let reversal = BaselineEntry {
            reversal_closes: 1,
            ..BaselineEntry::from_row(&traded("a", 1.0, false)).expect("scored")
        };
        assert_eq!(tp.net_r, reversal.net_r);
        assert_eq!(classify(&tp, &reversal), Some(MoveKind::Reshaped));
    }

    /// "The clock ran out" and "the setup broke" are different lessons at the same
    /// R, so they must be a detectable reshape. If `invalidation_closes` were left
    /// out of `shape()`, the two would compare equal and the corpus would bless a
    /// changed exit character as clean.
    #[test]
    fn expiry_vs_invalidation_close_is_a_change_of_shape() {
        let expiry = BaselineEntry {
            expiry_closes: 1,
            ..BaselineEntry::from_row(&traded("a", -0.25, false)).expect("scored")
        };
        let invalidation = BaselineEntry {
            invalidation_closes: 1,
            ..BaselineEntry::from_row(&traded("a", -0.25, false)).expect("scored")
        };
        assert_eq!(expiry.net_r, invalidation.net_r);
        assert_eq!(
            classify(&expiry, &invalidation),
            Some(MoveKind::Reshaped),
            "an expiry close reclassified as an invalidation close is a reshape"
        );
    }

    /// News-ON rows are flagged so calendar drift is never read as a code
    /// regression; news-OFF rows are not.
    #[test]
    fn news_on_movements_are_flagged_calendar_sensitive() {
        let base = Baseline::from_summary(
            &summary(vec![traded("on", 1.0, true), traded("off", 1.0, false)]),
            None,
        );
        let d = diff(
            &base,
            &summary(vec![traded("on", 2.0, true), traded("off", 2.0, false)]),
        );

        let flagged: Vec<&str> = d
            .moved
            .iter()
            .filter(|m| m.calendar_sensitive)
            .map(|m| m.fixture.as_str())
            .collect();
        assert_eq!(flagged, vec!["on"]);

        // `reproducible()` is the set a regression hunt starts from.
        let repro: Vec<&str> = d.reproducible().map(|m| m.fixture.as_str()).collect();
        assert_eq!(repro, vec!["off"]);
    }

    /// A fixture that failed to replay must not read as "unchanged". It leaves
    /// the fresh set entirely, so it surfaces as `removed` AND bumps `unscored`.
    #[test]
    fn a_fixture_that_stops_scoring_is_removed_not_unchanged() {
        let base = Baseline::from_summary(
            &summary(vec![traded("a", 0.5, false), traded("b", 0.5, false)]),
            None,
        );
        let d = diff(
            &base,
            &summary(vec![
                traded("a", 0.5, false),
                BatchResult::failed("b", "lock"),
            ]),
        );

        assert_eq!(d.unchanged, 1);
        assert_eq!(d.unscored, 1);
        assert_eq!(d.moved.len(), 1);
        assert_eq!(d.moved[0].kind, MoveKind::Removed);
        assert!(
            render(&d, 10).contains("INCOMPLETE"),
            "a partial comparison must not read as an answer"
        );
    }

    /// A brand-new fixture is `added`, and its `was` of 0.0 must not be confused
    /// with "was flat".
    #[test]
    fn a_new_fixture_is_added_not_a_zero_baseline() {
        let base = Baseline::from_summary(&summary(vec![traded("a", 0.5, false)]), None);
        let d = diff(
            &base,
            &summary(vec![traded("a", 0.5, false), traded("new", -0.7, false)]),
        );

        let m = d.moved.iter().find(|m| m.fixture == "new").expect("added");
        assert_eq!(m.kind, MoveKind::Added);
        assert_eq!(m.was, 0.0);
        assert_eq!(m.now, -0.7);
        assert_eq!(d.unchanged, 1);
    }

    /// A corpus captured across an engine change has no single version, and
    /// saying so beats picking one — a mixed baseline that *looks* coherent is
    /// worse than one that admits it isn't.
    #[test]
    fn a_mixed_version_corpus_records_no_version() {
        let mut old = traded("a", 1.0, false);
        if let Some(arm) = old.arm.as_mut() {
            arm.engine_version = Some("v112".into());
        }
        let s = summary(vec![old, traded("b", 1.0, false)]);
        assert_eq!(common_version(&s), None);

        // A consistent one is reported.
        let s2 = summary(vec![traded("a", 1.0, false), traded("b", 1.0, false)]);
        assert_eq!(common_version(&s2), Some("v113".into()));
    }

    /// A failed row's missing version must not make a coherent corpus look
    /// mixed — only *scored* rows are consulted.
    #[test]
    fn a_failed_row_does_not_poison_the_version() {
        let s = summary(vec![
            traded("a", 1.0, false),
            BatchResult::failed("b", "lock"),
        ]);
        assert_eq!(common_version(&s), Some("v113".into()));
    }

    /// The stored aggregate must agree with the entries, so a hand-edited
    /// baseline (a row deleted to silence a regression) is detectable.
    #[test]
    fn a_hand_edited_baseline_fails_its_consistency_check() {
        let mut base = Baseline::from_summary(
            &summary(vec![traded("a", 0.5, false), traded("b", -1.5, false)]),
            None,
        );
        assert!(base.is_self_consistent());
        assert_eq!(base.net_r, -1.0);

        base.entries.remove("b");
        assert!(
            !base.is_self_consistent(),
            "deleting a row must not silently shift the total"
        );
    }

    /// A baseline round-trips, and `deny_unknown_fields` means a stale key is a
    /// loud load error rather than a silently-ignored one.
    #[test]
    fn baseline_round_trips_and_rejects_unknown_keys() {
        let base = Baseline::from_summary(
            &summary(vec![traded("a", 0.5, true), not_taken("b")]),
            Some("v113".into()),
        );
        let json = serde_json::to_string_pretty(&base).expect("serialize");
        let back: Baseline = serde_json::from_str(&json).expect("round trip");
        assert_eq!(base, back);
        assert!(back.is_self_consistent());

        let polluted = json.replace("\"net_r\":", "\"stale_key\": 1, \"net_r\":");
        assert!(
            serde_json::from_str::<Baseline>(&polluted).is_err(),
            "an unrecognised key must be a load error, not silently dropped"
        );
    }

    /// Blessing sums in key order and the diff's `now` does too, so a
    /// bless-then-rediff reports exactly zero rather than a last-bit residue.
    ///
    /// Float addition isn't associative — this is not pedantry. The fixture
    /// gate already lost two digits to summation order once.
    #[test]
    fn the_aggregate_is_summed_in_a_reproducible_order() {
        // Values chosen so a different summation order really does differ.
        let rows = vec![
            traded("c", 0.1, false),
            traded("a", 0.2, false),
            traded("b", 0.3, false),
        ];
        let s = summary(rows);
        let base = Baseline::from_summary(&s, None);

        // Batch order is c, a, b; key order is a, b, c.
        assert_eq!(base.net_r, base.recomputed_net_r());
        let d = diff(&base, &s);
        assert_eq!(d.was, d.now, "same corpus must diff to exactly zero");
        assert_eq!(d.delta(), 0.0);
    }

    /// The rendered report says the things the scoping doc asks for.
    #[test]
    fn render_reports_aggregate_counts_and_the_worst_movers() {
        let base = Baseline::from_summary(
            &summary(vec![
                traded("trade-071", -1.0, false),
                traded("trade-118", 1.18, false),
            ]),
            Some("v113".into()),
        );
        let d = diff(
            &base,
            &summary(vec![
                traded("trade-071", -2.0, false),
                traded("trade-118", 0.0, false),
            ]),
        );
        let text = render(&d, 10);

        assert!(text.contains("vs v113 → v113"), "{text}");
        assert!(text.contains("net R:"), "{text}");
        assert!(text.contains("trade-071"), "{text}");
        assert!(text.contains("trade-118"), "{text}");
        assert!(
            text.contains("2 improved") || text.contains("0 improved"),
            "{text}"
        );
    }

    /// A sweep whose every mover is calendar-sensitive must say so at the
    /// summary level. The per-row `[calendar]` tag alone is too easy to miss
    /// across 291 rows, and the difference between "the engine changed" and "the
    /// calendar changed" is the whole point of the flag.
    #[test]
    fn a_wholly_calendar_sensitive_sweep_says_so_up_front() {
        let base = Baseline::from_summary(
            &summary(vec![traded("a", 1.0, true), traded("b", 1.0, true)]),
            None,
        );
        let all_news = render(
            &diff(
                &base,
                &summary(vec![traded("a", 2.0, true), traded("b", 2.0, true)]),
            ),
            10,
        );
        assert!(all_news.contains("0 reproducible"), "{all_news}");
        assert!(
            all_news.contains("none of this is necessarily a code change"),
            "{all_news}"
        );

        // One reproducible mover and the caveat is gone — there IS something to
        // explain by code.
        let mixed_base = Baseline::from_summary(
            &summary(vec![traded("a", 1.0, true), traded("b", 1.0, false)]),
            None,
        );
        let mixed = render(
            &diff(
                &mixed_base,
                &summary(vec![traded("a", 2.0, true), traded("b", 2.0, false)]),
            ),
            10,
        );
        assert!(
            mixed.contains("1 reproducible, 1 calendar-sensitive"),
            "{mixed}"
        );
        assert!(!mixed.contains("none of this is necessarily"), "{mixed}");
    }

    /// A clean sweep says so plainly rather than printing an empty list that
    /// reads like truncation.
    #[test]
    fn a_clean_sweep_says_nothing_moved() {
        let s = summary(vec![traded("a", 1.0, false)]);
        let text = render(&diff(&Baseline::from_summary(&s, None), &s), 10);
        assert!(text.contains("nothing moved"), "{text}");
        assert!(!text.contains("reproducible"), "{text}");
    }

    /// A long list is truncated with an explicit count — never silently.
    #[test]
    fn render_says_how_many_it_did_not_show() {
        let rows: Vec<BatchResult> = (0..20)
            .map(|i| traded(&format!("t{i:02}"), 1.0, false))
            .collect();
        let base = Baseline::from_summary(&summary(rows), None);
        let after: Vec<BatchResult> = (0..20)
            .map(|i| traded(&format!("t{i:02}"), 2.0, false))
            .collect();
        let text = render(&diff(&base, &summary(after)), 5);

        assert!(text.contains("… and 15 more"), "{text}");
    }

    /// An empty corpus diffs cleanly rather than erroring.
    #[test]
    fn an_empty_corpus_diffs_clean() {
        let base = Baseline::from_summary(&summary(Vec::new()), None);
        let d = diff(&base, &summary(Vec::new()));
        assert!(d.is_clean());
        assert_eq!((d.was, d.now, d.unchanged), (0.0, 0.0, 0));
        assert!(!render(&d, 5).contains("INCOMPLETE"));
    }
}
