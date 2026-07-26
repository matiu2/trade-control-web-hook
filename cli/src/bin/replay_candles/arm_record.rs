//! How a fixture was armed: the flag combination and the code version that
//! produced it.
//!
//! A fixture freezes exactly **one** arming variant, but nothing in the fixture
//! used to say *which*. That's fine for a single hand-picked regression case; it
//! breaks the moment the corpus holds several variants of the same trade. A
//! 6-cell entry-sensitivity grid (three entry rules × news on/off) is six
//! fixtures per trade that differ **only** by flags — on disk they'd be
//! indistinguishable except by filename convention, and a batch tool would have
//! to parse names to group them.
//!
//! So `--save` records the variant explicitly. A batch tool then groups by
//! `(trade_id, entry_rule, skip_calendar_bars)` from data, not from a filename.
//!
//! ## Why the versions matter
//!
//! `engine_version` / `tv_arm_version` tell you **which numbers predate a given
//! fix**. Entry-path behaviour changes often (the QM/v2 confirmation fix, the
//! break-and-close origin-side rule, the SL-spread-floor salvage), and each one
//! can legitimately move R on historical setups. Without a version stamp, a
//! corpus of 291 saved Net R figures silently mixes pre- and post-fix numbers
//! and the aggregate means nothing. With it, a blessed baseline is the triple
//! `(corpus, engine_version, aggregate)` — the thing a later run diffs against.
//!
//! ## Provenance is not just bookkeeping
//!
//! `chart_symbol` is recorded **broker-qualified** (`TRADENATION:EURUSD`, not a
//! bare `EURUSD`) because a bare TradingView symbol silently resolves to the
//! OANDA feed. A fixture captured off the wrong feed produces a plausible number
//! that quietly dilutes an aggregate — and at 291 trades nobody will spot it by
//! eye. Recording the qualified symbol makes a bad capture *findable later*
//! instead of invisible.

use serde::{Deserialize, Serialize};

/// Which entry rule the trade was armed with. These are mutually exclusive ways
/// of deciding *when* to place the order, which is exactly the axis the
/// entry-sensitivity grid varies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum EntryRule {
    /// The full gated path: break-and-close prep, then retest prep, then enter.
    #[default]
    Normal,
    /// `--skip-bcr` — both preps skipped; enter on the first qualifying signal.
    SkipBcr,
    /// `--strategy-v2` — the Quasimodo limit leg, no preps, confirming candle.
    StrategyV2,
    /// Something else (a future strategy, or a flag combination none of the above
    /// describes). Carries the raw label so an unrecognised variant is still
    /// *recorded* rather than silently mislabelled as `normal`.
    Other(String),
}

impl EntryRule {
    /// The grid-column label. Stable across versions — `batch.rs` groups on it.
    pub fn label(&self) -> &str {
        match self {
            Self::Normal => "normal",
            Self::SkipBcr => "skip-bcr",
            Self::StrategyV2 => "strategy-v2",
            Self::Other(s) => s,
        }
    }

    /// Parse the `--arm-entry-rule` value. An unrecognised label is preserved
    /// verbatim as [`Self::Other`] rather than coerced to `Normal`: a future
    /// strategy flag must show up as itself in the corpus, not silently
    /// masquerade as the default and corrupt a grid column.
    ///
    /// `None` (flag absent) means the fixture was saved without arm info, which
    /// is the plain gated path — `Normal`. The inverse of [`Self::label`], so
    /// `parse(x.label()) == x` for every variant.
    pub fn parse(label: Option<&str>) -> Self {
        match label {
            None | Some("normal") => Self::Normal,
            Some("skip-bcr") => Self::SkipBcr,
            Some("strategy-v2") => Self::StrategyV2,
            Some(other) => Self::Other(other.to_string()),
        }
    }
}

/// The arming provenance of a saved fixture: which variant, from which chart, at
/// which code version.
///
/// Every field is journalling/grouping only — **none of it is read back into the
/// replay**. `--test-mode` reconstructs the run from the frozen plan + candles;
/// this block explains *where that plan came from*.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ArmRecord {
    /// Which entry rule — the grid's column axis.
    pub entry_rule: EntryRule,
    /// `--skip-calendar-bars`: news windows suppressed. The grid's row axis.
    ///
    /// Load-bearing to record, because it is **not inferable from the plan**: a
    /// plan with no pause rules could equally mean "calendar ran, no events in
    /// the window" or "calendar skipped entirely".
    #[serde(default)]
    pub skip_calendar_bars: bool,
    /// `--skip-golden`: the golden-candle quality gate waived.
    #[serde(default)]
    pub skip_golden: bool,
    /// The `--start` cursor as the operator typed it, verbatim. Kept as a string
    /// (not parsed) so the exact spelling round-trips for a re-arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// Broker the plan was armed against (`tradenation` / `oanda`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broker: Option<String>,
    /// The **broker-qualified** TradingView symbol the geometry was read from,
    /// e.g. `TRADENATION:EURUSD`. Unqualified is a bug waiting to happen — see
    /// the module doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chart_symbol: Option<String>,
    /// `git describe` of the tv-arm build that armed the plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tv_arm_version: Option<String>,
    /// `git describe` of the replay-candles build that produced the outcome. This
    /// is the one that tells you whether a saved Net R predates an engine fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
    /// Free-form pointer back to the journal page this fixture documents, e.g.
    /// `trade-124`. Lets the corpus be cross-referenced with the journal in both
    /// directions, and makes "which pages still lack a fixture?" answerable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_ref: Option<String>,
}

impl ArmRecord {
    /// A stable grouping key for batch analysis: one grid **cell** is one
    /// `(entry_rule, news-on/off)` pair. Two fixtures with the same key are the
    /// same cell of the same grid and should be directly comparable.
    ///
    /// This is what lets `batch.rs` group fixtures by cell from **data** instead
    /// of parsing filenames — the whole point of recording the arm block.
    pub fn cell_key(&self) -> String {
        let news = if self.skip_calendar_bars { "off" } else { "on" };
        format!("{}/news-{news}", self.entry_rule.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_rule_parses_the_known_labels() {
        assert_eq!(EntryRule::parse(None), EntryRule::Normal);
        assert_eq!(EntryRule::parse(Some("normal")), EntryRule::Normal);
        assert_eq!(EntryRule::parse(Some("skip-bcr")), EntryRule::SkipBcr);
        assert_eq!(EntryRule::parse(Some("strategy-v2")), EntryRule::StrategyV2);
    }

    /// `parse` is the inverse of `label` for every variant — the property that
    /// lets a fixture's recorded label round-trip through a batch tool unchanged.
    #[test]
    fn parse_is_the_inverse_of_label() {
        for rule in [
            EntryRule::Normal,
            EntryRule::SkipBcr,
            EntryRule::StrategyV2,
            EntryRule::Other("scaled-exit-80-90".into()),
        ] {
            assert_eq!(EntryRule::parse(Some(rule.label())), rule);
        }
    }

    /// An unrecognised label is preserved, not coerced to `normal` — otherwise a
    /// future strategy flag would silently pollute the `normal` grid column.
    #[test]
    fn unknown_label_is_preserved_not_coerced() {
        let rule = EntryRule::parse(Some("skip-bcr+strategy-v2"));
        assert_eq!(rule.label(), "skip-bcr+strategy-v2");
        assert_ne!(rule, EntryRule::Normal);
    }

    /// The six cells of one trade's grid must produce six distinct keys.
    #[test]
    fn the_six_grid_cells_have_distinct_keys() {
        let mut keys = Vec::new();
        for rule in [EntryRule::Normal, EntryRule::SkipBcr, EntryRule::StrategyV2] {
            for skip_calendar_bars in [false, true] {
                keys.push(
                    ArmRecord {
                        entry_rule: rule.clone(),
                        skip_calendar_bars,
                        ..Default::default()
                    }
                    .cell_key(),
                );
            }
        }
        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), 6, "six cells, six keys: {keys:?}");
        assert!(keys.contains(&"normal/news-on".to_string()));
        assert!(keys.contains(&"strategy-v2/news-off".to_string()));
    }

    /// Round-trips through JSON, and the labels are the stable kebab-case forms a
    /// batch tool groups on.
    #[test]
    fn arm_record_json_round_trips_with_kebab_labels() {
        let arm = ArmRecord {
            entry_rule: EntryRule::SkipBcr,
            skip_calendar_bars: true,
            skip_golden: false,
            start: Some("2026-07-17T17:00:00+10:00".into()),
            broker: Some("tradenation".into()),
            chart_symbol: Some("TRADENATION:EURUSD".into()),
            tv_arm_version: Some("v113-4-gabc123".into()),
            engine_version: Some("v113-4-gabc123".into()),
            journal_ref: Some("trade-124".into()),
        };
        let json = serde_json::to_string_pretty(&arm).unwrap();
        assert!(json.contains("\"skip-bcr\""), "kebab-case label: {json}");
        let back: ArmRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(arm, back);
    }

    /// An `Other` variant survives the round-trip with its raw label — a future
    /// strategy flag records rather than degrades.
    #[test]
    fn unknown_entry_rule_round_trips_verbatim() {
        let arm = ArmRecord {
            entry_rule: EntryRule::Other("scaled-exit-80-90".into()),
            ..Default::default()
        };
        let back: ArmRecord = serde_json::from_str(&serde_json::to_string(&arm).unwrap()).unwrap();
        assert_eq!(back.entry_rule.label(), "scaled-exit-80-90");
        assert_eq!(back.cell_key(), "scaled-exit-80-90/news-on");
    }

    /// Defaults serialize thin: only `entry_rule` (plus the two bools) is
    /// mandatory, so a minimally-described fixture stays readable.
    #[test]
    fn default_arm_record_omits_optional_keys() {
        let json = serde_json::to_string(&ArmRecord::default()).unwrap();
        for absent in ["start", "broker", "chart_symbol", "journal_ref"] {
            assert!(!json.contains(absent), "{absent} must be omitted: {json}");
        }
        assert!(json.contains("normal"));
    }
}
