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
    /// `--strategy-v2 --qm-entry market` — same as [`Self::StrategyV2`] except
    /// the QM leg enters at **market** on the confirmation bar instead of resting
    /// as a limit at the signal level.
    ///
    /// Its own column rather than a flavour of `strategy-v2`: the two answer
    /// different questions. The limit leg asks "does waiting for the pullback pay
    /// for the fills it misses?", the market leg "is the confirmation candle
    /// alone enough?". Folding them together would average a fill-rate
    /// difference into a returns difference and hide both.
    StrategyV2QmMarket,
    /// `--strategy-v2 --qm-entry stop` — the QM leg rests as a **stop** at
    /// signal_low − buffer instead of a limit at the level.
    ///
    /// Not one of the standard grid cells, but reachable by hand, so it parses as
    /// itself rather than degrading to [`Self::Other`].
    StrategyV2QmStop,
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
            Self::StrategyV2QmMarket => "strategy-v2-qm-market",
            Self::StrategyV2QmStop => "strategy-v2-qm-stop",
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
            Some("strategy-v2-qm-market") => Self::StrategyV2QmMarket,
            Some("strategy-v2-qm-stop") => Self::StrategyV2QmStop,
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
    /// `--skip-reversals`: both reversal-closes dropped. The grid's **third**
    /// axis.
    ///
    /// Not inferable from the plan either (see [`Self::skip_calendar_bars`] for
    /// the same argument): a plan with no `07-close-on-sr-reversal` could mean
    /// the flag was passed, or simply that no S/R lines were drawn.
    ///
    /// `#[serde(default)]` is what keeps the pre-flag corpus readable — every
    /// fixture captured before this axis existed had reversals **on**, which is
    /// exactly `false`.
    #[serde(default)]
    pub skip_reversals: bool,
    /// The `--start` cursor as the operator typed it, verbatim. Kept as a string
    /// (not parsed) so the exact spelling round-trips for a re-arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// Which broker's candles the replay pulled (`tradenation` / `oanda`) — i.e.
    /// `--source`, **not** necessarily the broker the plan was armed against.
    ///
    /// Named for what it actually holds. `tv-arm … replay` derives `--source` from
    /// the resolved arming broker, so in the normal flow they agree — but a
    /// standalone `replay-candles --plan … --source oanda` on a TradeNation-armed
    /// plan records `oanda` here, which is the truth worth having: the numbers came
    /// off the OANDA feed. Mislabelling this "the arming broker" would hide exactly
    /// the wrong-feed capture that `chart_symbol` is qualified to catch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candle_source: Option<String>,
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
    /// `(entry_rule, news-on/off, rev-on/off)` triple. Two fixtures with the
    /// same key are the same cell of the same grid and should be directly
    /// comparable.
    ///
    /// This is what lets `batch.rs` group fixtures by cell from **data** instead
    /// of parsing filenames — the whole point of recording the arm block.
    ///
    /// ## The reversal axis is suffix-only, on purpose
    ///
    /// A reversals-**on** cell keeps its historical key verbatim
    /// (`normal/news-on`), and only the reversals-off twin carries the
    /// `/rev-off` suffix. Appending `/rev-on` to the majority case would rename
    /// every key in the existing corpus, so a blessed baseline would compare a
    /// `normal/news-on` row against nothing and read as a wholesale grid change
    /// rather than a new column. Same reasoning as the `#[serde(default)]` on
    /// the field: absent means reversals were on.
    pub fn cell_key(&self) -> String {
        let news = if self.skip_calendar_bars { "off" } else { "on" };
        let rev = if self.skip_reversals { "/rev-off" } else { "" };
        format!("{}/news-{news}{rev}", self.entry_rule.label())
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
        assert_eq!(
            EntryRule::parse(Some("strategy-v2-qm-market")),
            EntryRule::StrategyV2QmMarket
        );
    }

    /// `strategy-v2-qm-market` must NOT collapse into `strategy-v2`.
    ///
    /// The labels share a prefix, so any parse written with `starts_with` (or a
    /// `match` arm ordered the other way) would swallow the market cell into the
    /// limit column — two variants averaged under one name, with nothing
    /// reporting a problem.
    #[test]
    fn the_qm_market_label_is_not_swallowed_by_the_strategy_v2_prefix() {
        assert_ne!(
            EntryRule::parse(Some("strategy-v2-qm-market")),
            EntryRule::StrategyV2
        );
        assert_eq!(
            EntryRule::parse(Some("strategy-v2-qm-market")).label(),
            "strategy-v2-qm-market"
        );
    }

    /// `parse` is the inverse of `label` for every variant — the property that
    /// lets a fixture's recorded label round-trip through a batch tool unchanged.
    #[test]
    fn parse_is_the_inverse_of_label() {
        for rule in [
            EntryRule::Normal,
            EntryRule::SkipBcr,
            EntryRule::StrategyV2,
            EntryRule::StrategyV2QmMarket,
            EntryRule::StrategyV2QmStop,
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

    /// The sixteen cells of one trade's grid must produce sixteen distinct keys.
    ///
    /// Collisions are the failure that matters: two cells sharing a key means a
    /// batch tool averages two different variants into one row and reports it as
    /// a single confident number.
    #[test]
    fn the_sixteen_grid_cells_have_distinct_keys() {
        let mut keys = Vec::new();
        for rule in [
            EntryRule::Normal,
            EntryRule::SkipBcr,
            EntryRule::StrategyV2,
            EntryRule::StrategyV2QmMarket,
        ] {
            for skip_calendar_bars in [false, true] {
                for skip_reversals in [false, true] {
                    keys.push(
                        ArmRecord {
                            entry_rule: rule.clone(),
                            skip_calendar_bars,
                            skip_reversals,
                            ..Default::default()
                        }
                        .cell_key(),
                    );
                }
            }
        }
        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), 16, "sixteen cells, sixteen keys: {keys:?}");
        assert!(keys.contains(&"normal/news-on".to_string()));
        assert!(keys.contains(&"strategy-v2/news-off".to_string()));
        assert!(keys.contains(&"strategy-v2-qm-market/news-on".to_string()));
        assert!(keys.contains(&"normal/news-on/rev-off".to_string()));
        assert!(keys.contains(&"strategy-v2-qm-market/news-off/rev-off".to_string()));
    }

    /// A reversals-**on** record keeps the exact key it had before the reversal
    /// axis existed.
    ///
    /// This is the compatibility pin for the whole corpus: every fixture already
    /// on disk deserialises with `skip_reversals: false` (serde default), and if
    /// that suffixed its key to `normal/news-on/rev-on` then a blessed baseline
    /// would find no row matching `normal/news-on` and read the entire grid as
    /// having changed, rather than gaining a column.
    #[test]
    fn reversals_on_keeps_the_pre_axis_cell_key() {
        let on = ArmRecord {
            entry_rule: EntryRule::Normal,
            skip_calendar_bars: false,
            skip_reversals: false,
            ..Default::default()
        };
        assert_eq!(on.cell_key(), "normal/news-on");

        // …and the twin is a strict suffix of it, so the pair is greppable.
        let off = ArmRecord {
            skip_reversals: true,
            ..on.clone()
        };
        assert_eq!(off.cell_key(), "normal/news-on/rev-off");
        assert!(off.cell_key().starts_with(&on.cell_key()));
    }

    /// A fixture saved before the reversal axis existed must read back as
    /// reversals-**on**, not fail to parse and not default to "off".
    ///
    /// Those fixtures were captured with the reversal-closes live, so `false` is
    /// the historically accurate value — reading them as `rev-off` would file
    /// hundreds of reversal-on results under the reversal-off column.
    #[test]
    fn a_pre_axis_meta_json_reads_back_as_reversals_on() {
        let legacy = r#"{"entry_rule":"skip-bcr","skip_calendar_bars":true}"#;
        let back: ArmRecord = serde_json::from_str(legacy).expect("legacy meta still parses");
        assert!(
            !back.skip_reversals,
            "absent field means reversals were ON when this was captured"
        );
        assert_eq!(back.cell_key(), "skip-bcr/news-off");
    }

    /// Round-trips through JSON, and the labels are the stable kebab-case forms a
    /// batch tool groups on.
    #[test]
    fn arm_record_json_round_trips_with_kebab_labels() {
        let arm = ArmRecord {
            entry_rule: EntryRule::SkipBcr,
            skip_calendar_bars: true,
            skip_golden: false,
            skip_reversals: false,
            start: Some("2026-07-17T17:00:00+10:00".into()),
            candle_source: Some("tradenation".into()),
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

    /// The serde spelling and the hand-written `label()` must agree.
    ///
    /// They are two independent renderings of the same variant: serde derives
    /// `strategy-v2-qm-market` from `rename_all = "kebab-case"`, `label()` spells
    /// it out. If they drift, a fixture serialises under one name and groups
    /// under the other, and the grid quietly gains a phantom column.
    #[test]
    fn the_qm_market_serde_name_matches_its_label() {
        let arm = ArmRecord {
            entry_rule: EntryRule::StrategyV2QmMarket,
            ..Default::default()
        };
        let json = serde_json::to_string(&arm).unwrap();
        assert!(
            json.contains("\"strategy-v2-qm-market\""),
            "serde must spell it the same as label(): {json}"
        );
        let back: ArmRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entry_rule, EntryRule::StrategyV2QmMarket);
        assert_eq!(back.entry_rule.label(), "strategy-v2-qm-market");
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
    ///
    /// The names below are checked against the struct's real fields by
    /// [`every_optional_field_is_named_in_the_omission_test`] — this list used to
    /// contain `"broker"`, a field renamed to `candle_source` long before, so the
    /// test was asserting the absence of a string that could never appear while
    /// the actual field went untested.
    #[test]
    fn default_arm_record_omits_optional_keys() {
        let json = serde_json::to_string(&ArmRecord::default()).unwrap();
        for absent in OPTIONAL_FIELDS {
            assert!(!json.contains(absent), "{absent} must be omitted: {json}");
        }
        assert!(json.contains("normal"));
    }

    /// Every `Option` field on [`ArmRecord`], i.e. every key that must vanish
    /// from a default record.
    const OPTIONAL_FIELDS: [&str; 6] = [
        "start",
        "candle_source",
        "chart_symbol",
        "tv_arm_version",
        "engine_version",
        "journal_ref",
    ];

    /// Guard against the list above going stale, which is how `"broker"` survived
    /// a rename: serialize a record with EVERY optional field populated, and
    /// require the list to name each optional key the struct actually emits.
    ///
    /// A renamed or newly-added optional field now fails here instead of quietly
    /// dropping out of coverage.
    #[test]
    fn every_optional_field_is_named_in_the_omission_test() {
        let full = ArmRecord {
            entry_rule: EntryRule::Normal,
            skip_calendar_bars: false,
            skip_golden: false,
            skip_reversals: false,
            start: Some("2026-07-17T17:00:00+10:00".into()),
            candle_source: Some("tradenation".into()),
            chart_symbol: Some("TRADENATION:EURUSD".into()),
            tv_arm_version: Some("v113".into()),
            engine_version: Some("v113".into()),
            journal_ref: Some("trade-124".into()),
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&full).unwrap()).unwrap();
        let keys: Vec<String> = value
            .as_object()
            .expect("an object")
            .keys()
            .filter(|k| {
                !matches!(
                    k.as_str(),
                    "entry_rule" | "skip_calendar_bars" | "skip_golden" | "skip_reversals"
                )
            })
            .cloned()
            .collect();

        for key in &keys {
            assert!(
                OPTIONAL_FIELDS.contains(&key.as_str()),
                "optional field {key:?} is not covered by OPTIONAL_FIELDS — add it \
                 (this is the check that would have caught the `broker` → \
                 `candle_source` rename)"
            );
        }
        assert_eq!(
            keys.len(),
            OPTIONAL_FIELDS.len(),
            "OPTIONAL_FIELDS lists {} names but the struct emits {}: {keys:?}",
            OPTIONAL_FIELDS.len(),
            keys.len()
        );
    }
}
