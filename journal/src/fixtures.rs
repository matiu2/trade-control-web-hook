//! Which saved replay-fixtures belong to a plan.
//!
//! The corpus under `replay-fixtures/` is a flat set of per-cell directories
//! (`<base>-<entry-rule>-news-<on|off>/`), each holding a `meta.json` with the
//! instrument, granularity and window start the cell was captured for. Nothing
//! in that file names the plan it came from, so this module answers "does the
//! selected trade have a fixture?" by **matching on what the fixture does
//! record**.
//!
//! # Why not match on the name, or on `journal_ref`
//!
//! Two exact links exist on paper and neither is usable today:
//!
//! * **The directory name.** `journal` captures with `--fixture-name
//!   <trade_id>`, so a journal-captured cell is `hs-nzd-chf-99d6bd00-…`. But
//!   tv-arm's own default derives `<instrument>-<granularity>-<date>`, and
//!   111 of the 125 cells in the corpus were captured that way — matching on
//!   the name alone reports "none" for almost every plan that in fact has one.
//! * **`meta.arm.journal_ref`.** `ReplayArgs::trade_ref` exists precisely to
//!   record this ("makes the corpus cross-referenceable with the journal in
//!   both directions"), but nothing passes `--trade-ref` — tv-arm's capture
//!   path never emits it, so the field is `null` in every cell on disk.
//!
//! A third link looks promising and is **a trap**: every cell's `plan.json`
//! carries a `trade_id`. It is not the journal plan's id. Each cell is
//! independently re-armed from the chart and mints a fresh id, so the 111 cells
//! in the corpus hold 111 *distinct* trade_ids — and none of them matches any
//! of the live plans (verified 2026-08-07: overlap was exactly zero against 46
//! plans). Matching on it would report "no fixture" for everything, so don't
//! reach for it when this heuristic annoys you.
//!
//! So the match is **instrument + granularity + start within
//! [`MATCH_WINDOW_HOURS`], nearest wins**. All three are fields `meta.json` has
//! always carried, which makes every pre-existing cell matchable with no
//! schema change and no re-capture. If `--trade-ref` is wired later, an exact
//! `journal_ref` hit should be preferred over this and fall back to it.
//!
//! # Instrument spelling is NOT string equality
//!
//! The corpus holds both `NZD/CHF` (TradeNation) and `NZD_CHF` (OANDA) — the
//! same asset, captured against two brokers. A plan carries whichever spelling
//! its own broker uses. Comparing the raw strings splits one asset in two and
//! silently reports "no fixture" for the other broker's cells, so both sides
//! are canonicalised through `instrument-lookup` first. An id the catalog
//! doesn't know falls back to a case-folded raw compare rather than matching
//! nothing.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// How far a fixture's window start may sit from the plan's arm time and still
/// be considered the same setup. A capture's `start` is the replay window's
/// beginning, which is deliberately earlier than the arm instant (it reaches
/// back for warm-up and for the pattern's own structure), so an exact match is
/// never expected — but a *different* setup on the same instrument and
/// timeframe is normally days apart, not hours.
const MATCH_WINDOW_HOURS: i64 = 24;

/// The subset of a cell's `meta.json` this match needs. Deliberately NOT
/// `deny_unknown_fields`: the real [`FixtureMeta`] carries more (source,
/// end, message, the whole `arm` record) and grows over time, and a reader
/// that only asks "which plan is this?" must not fail to load a cell because
/// the capture side added a field.
#[derive(Debug, Clone, Deserialize)]
struct CellMeta {
    instrument: String,
    granularity: String,
    start: DateTime<Utc>,
}

/// One saved fixture cell on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    /// Directory name, e.g. `aud-cad-h1-2026-07-22-normal-news-off`.
    pub name: String,
    /// Instrument exactly as recorded (broker spelling, not canonicalised).
    pub instrument: String,
    pub granularity: String,
    pub start: DateTime<Utc>,
}

impl Cell {
    /// The setup this cell belongs to: the directory name with its
    /// `-<entry-rule>-news-<on|off>` suffix removed. Cells of one capture share
    /// a base, which is what makes "8 cells saved" one fact rather than eight.
    ///
    /// Splitting on `-news-` is what makes this robust to the entry-rule set
    /// changing: the grid has grown from three rules to four
    /// (`strategy-v2-qm-market` was added), and every cell — whatever its rule
    /// — ends in `-news-on` or `-news-off`. A name without that marker isn't a
    /// grid cell, so it stands alone as its own base.
    pub fn base(&self) -> &str {
        match self.name.rfind("-news-") {
            Some(i) => {
                // Trim the entry-rule segment too: `<base>-<rule>-news-<x>`.
                let head = &self.name[..i];
                match head.rfind('-') {
                    // Only trim when what follows looks like a rule segment,
                    // never past the start of the name.
                    Some(j) if j > 0 => strip_entry_rule(head, j),
                    _ => head,
                }
            }
            None => &self.name,
        }
    }
}

/// The grid's entry-rule slugs. Kept as data rather than parsed positionally
/// because rule slugs contain their own dashes (`skip-bcr`,
/// `strategy-v2-qm-market`), so "the last dash-separated segment" is not the
/// rule.
///
/// Order is **not** load-bearing: matching is `strip_suffix`, which anchors at
/// the end, and no slug here is a suffix of another (`strategy-v2-qm-market`
/// *starts with* `strategy-v2` but does not end with it). Listing longest-first
/// is a readability convention only — a shortest-first list produces identical
/// bases for every name in the grid. If a future rule is ever added that IS a
/// suffix of another (say `v2` alongside `strategy-v2`), order becomes
/// load-bearing and this comment is wrong.
const ENTRY_RULES: [&str; 4] = ["strategy-v2-qm-market", "strategy-v2", "skip-bcr", "normal"];

/// Remove a trailing entry-rule segment from `head` (already `-news-`-trimmed).
/// `fallback_dash` is the last dash position, used when the rule isn't one we
/// know — a future rule slug still yields a usable base rather than a name that
/// silently fails to group with its siblings.
fn strip_entry_rule(head: &str, fallback_dash: usize) -> &str {
    for rule in ENTRY_RULES {
        if let Some(stem) = head.strip_suffix(rule)
            && let Some(stem) = stem.strip_suffix('-')
        {
            return stem;
        }
    }
    &head[..fallback_dash]
}

/// A plan's fixture status, as shown in the info bar.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// No cell matched this plan.
    None,
    /// `cells` cells of one capture matched.
    Saved { base: String, cells: usize },
}

impl Status {
    /// Compact info-bar label.
    pub fn label(&self) -> String {
        match self {
            Status::None => "no fixture".to_string(),
            Status::Saved { cells, .. } => format!("fixture {cells} ✓"),
        }
    }

    pub fn is_saved(&self) -> bool {
        matches!(self, Status::Saved { .. })
    }
}

/// Read every fixture cell under `dir`. A directory without a readable
/// `meta.json` is skipped (the `.spec.json` files that sit alongside the cells
/// are not directories and never appear here); an unreadable `dir` yields an
/// empty corpus, which the caller renders as "no fixture" rather than an error
/// — a missing `replay-fixtures/` is a normal state for a fresh checkout, not
/// a failure worth interrupting the TUI for.
pub fn scan(dir: &Path) -> Vec<Cell> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut cells: Vec<Cell> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| read_cell(&e.path()))
        .collect();
    cells.sort_by(|a, b| a.name.cmp(&b.name));
    cells
}

/// Parse one cell directory's `meta.json` into a [`Cell`].
fn read_cell(path: &Path) -> Option<Cell> {
    let raw = std::fs::read_to_string(path.join("meta.json")).ok()?;
    let meta: CellMeta = serde_json::from_str(&raw).ok()?;
    Some(Cell {
        name: path.file_name()?.to_string_lossy().to_string(),
        instrument: meta.instrument,
        granularity: meta.granularity,
        start: meta.start,
    })
}

/// The fixture status for a plan armed at `armed_at` on `instrument` /
/// `granularity`.
///
/// Cells are kept when the instrument and granularity both match and the
/// capture's `start` is within [`MATCH_WINDOW_HOURS`] of `armed_at`; of the
/// **setups** that qualify, the one whose start is nearest `armed_at` wins, and
/// its whole cell group is reported. Grouping before choosing is what makes the
/// count meaningful — picking the single nearest *cell* would always report 1.
///
/// A plan with no `armed_at` cannot be placed in time, so it matches nothing
/// rather than claiming the first same-instrument capture.
pub fn status_for(
    cells: &[Cell],
    instrument: &str,
    granularity: &str,
    armed_at: Option<&str>,
) -> Status {
    let Some(armed) = armed_at.and_then(parse_ts) else {
        return Status::None;
    };
    let want = canonical_instrument(instrument);
    let window = chrono::Duration::hours(MATCH_WINDOW_HOURS);

    let candidates: Vec<&Cell> = cells
        .iter()
        .filter(|c| c.granularity.eq_ignore_ascii_case(granularity))
        .filter(|c| canonical_instrument(&c.instrument) == want)
        .filter(|c| (c.start - armed).abs() <= window)
        .collect();

    // Nearest setup, not nearest cell: pick the winning base by its closest
    // cell, then report every cell sharing that base.
    let Some(best) = candidates
        .iter()
        .min_by_key(|c| (c.start - armed).abs().num_seconds().abs())
    else {
        return Status::None;
    };
    let base = best.base().to_string();
    let cells = candidates.iter().filter(|c| c.base() == base).count();
    Status::Saved { base, cells }
}

/// Parse an RFC3339 instant, tolerating the sub-second precision plan exports
/// carry (`…T09:12:10.392142272Z`).
fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Canonical, broker-agnostic id for an instrument, so a plan's `NZD_CHF`
/// matches a fixture's `NZD/CHF`. Unknown ids (or a malformed user overlay)
/// fall back to the upper-cased raw string with separators removed, which still
/// unifies the two FX spellings even when the catalog can't be consulted.
fn canonical_instrument(raw: &str) -> String {
    use instrument_lookup::{Broker, by_broker_symbol};
    for broker in [Broker::Oanda, Broker::TradeNation] {
        if let Ok(Some(asset)) = by_broker_symbol(broker, raw) {
            return asset.id.to_uppercase();
        }
    }
    raw.to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// The corpus directory: `replay-fixtures/` beside the repo. Honours
/// `TRADE_CONTROL_FIXTURES_DIR` so a worktree (whose own `replay-fixtures/`
/// holds only the committed cells) can be pointed at the full corpus — the
/// same override the replay CLI's `--fixtures-dir` serves.
pub fn default_dir() -> PathBuf {
    if let Ok(p) = std::env::var("TRADE_CONTROL_FIXTURES_DIR") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../replay-fixtures")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(name: &str, instrument: &str, granularity: &str, start: &str) -> Cell {
        Cell {
            name: name.to_string(),
            instrument: instrument.to_string(),
            granularity: granularity.to_string(),
            start: parse_ts(start).expect("test timestamp parses"),
        }
    }

    /// The eight cells of one capture share a base, so the status is "8 cells
    /// of one setup", not eight separate setups. This is the whole reason
    /// `base()` exists.
    #[test]
    fn cells_of_one_capture_share_a_base() {
        let names = [
            "aud-cad-h1-2026-07-22-normal-news-off",
            "aud-cad-h1-2026-07-22-normal-news-on",
            "aud-cad-h1-2026-07-22-skip-bcr-news-off",
            "aud-cad-h1-2026-07-22-skip-bcr-news-on",
            "aud-cad-h1-2026-07-22-strategy-v2-news-off",
            "aud-cad-h1-2026-07-22-strategy-v2-news-on",
            "aud-cad-h1-2026-07-22-strategy-v2-qm-market-news-off",
            "aud-cad-h1-2026-07-22-strategy-v2-qm-market-news-on",
        ];
        for n in names {
            let c = cell(n, "AUD_CAD", "h1", "2026-07-22T10:00:00Z");
            assert_eq!(
                c.base(),
                "aud-cad-h1-2026-07-22",
                "{n} must reduce to the shared base"
            );
        }
    }

    /// The multi-dash rule slug reduces to the same base as its siblings.
    /// A naive "trim the last dash-separated segment" would leave
    /// `…-strategy-v2-qm` and split one capture into two setups.
    ///
    /// Note this does NOT pin the ordering of [`ENTRY_RULES`] — `strip_suffix`
    /// anchors at the end, so shortest-first yields the same answer. Reordering
    /// the list is not a behaviour change (verified by mutation).
    #[test]
    fn multi_dash_entry_rule_reduces_to_the_shared_base() {
        let c = cell(
            "eur-usd-h1-2026-07-22-strategy-v2-qm-market-news-on",
            "EUR_USD",
            "h1",
            "2026-07-22T20:00:00Z",
        );
        assert_eq!(c.base(), "eur-usd-h1-2026-07-22");
    }

    /// A journal-captured cell is named after the trade_id, which contains
    /// dashes of its own — the base must keep the whole id intact.
    #[test]
    fn trade_id_named_cells_keep_the_full_id_as_base() {
        let c = cell(
            "hs-nzd-chf-99d6bd00-strategy-v2-news-off",
            "NZD_CHF",
            "h1",
            "2026-06-19T00:00:00Z",
        );
        assert_eq!(c.base(), "hs-nzd-chf-99d6bd00");
    }

    /// A rule slug this build doesn't know (the grid has grown before, from
    /// three rules to four) must still reduce to a usable base by falling back
    /// to the last dash-separated segment — a single-segment unknown rule
    /// groups correctly, rather than each cell becoming its own setup.
    #[test]
    fn an_unknown_single_segment_rule_still_groups() {
        let off = cell(
            "aud-cad-h1-2026-07-22-brandnew-news-off",
            "AUD_CAD",
            "h1",
            "2026-07-22T10:00:00Z",
        );
        let on = cell(
            "aud-cad-h1-2026-07-22-brandnew-news-on",
            "AUD_CAD",
            "h1",
            "2026-07-22T10:00:00Z",
        );
        assert_eq!(off.base(), "aud-cad-h1-2026-07-22");
        assert_eq!(off.base(), on.base(), "both cells group as one setup");
    }

    /// A name that isn't a grid cell has no `-news-` marker and stands alone.
    #[test]
    fn a_non_grid_name_is_its_own_base() {
        let c = cell("coffee-sad", "Coffee", "m15", "2026-07-20T13:00:00Z");
        assert_eq!(c.base(), "coffee-sad");
    }

    /// Known limitation, pinned so it isn't mistaken for a regression: a
    /// hand-named fixture that happens to contain the literal `-news-`
    /// (`uk-100-news-blackout-…` is real, in the corpus) is split there and
    /// yields a short base. Harmless — the base is only a grouping key, never
    /// shown, and a one-off fixture groups alone either way; the match itself
    /// still runs on instrument + granularity + time. It would only matter if
    /// two such fixtures collided on the truncated base AND fell in the same
    /// 24h window, which would merge their counts.
    #[test]
    fn a_hand_named_fixture_containing_news_truncates_its_base() {
        let c = cell(
            "uk-100-news-blackout-rentry-close-on-reversal",
            "UK 100",
            "h1",
            "2026-07-13T11:00:00Z",
        );
        assert_eq!(c.base(), "uk", "documents today's behaviour, not an ideal");
        // The important part: it still matches its own plan.
        let status = status_for(&[c], "UK 100", "h1", Some("2026-07-13T12:00:00Z"));
        assert!(status.is_saved(), "{status:?}");
    }

    /// The headline case: a plan armed 48 minutes after its capture's window
    /// start matches, and reports the full cell count.
    #[test]
    fn matches_a_capture_armed_within_the_window() {
        let cells: Vec<Cell> = ["normal-news-off", "normal-news-on", "skip-bcr-news-off"]
            .iter()
            .map(|s| {
                cell(
                    &format!("aud-cad-h1-2026-07-22-{s}"),
                    "AUD_CAD",
                    "h1",
                    "2026-07-22T10:00:00Z",
                )
            })
            .collect();
        let status = status_for(
            &cells,
            "AUD_CAD",
            "h1",
            Some("2026-07-22T09:12:10.392142272Z"),
        );
        assert_eq!(
            status,
            Status::Saved {
                base: "aud-cad-h1-2026-07-22".into(),
                cells: 3
            }
        );
        assert_eq!(status.label(), "fixture 3 ✓");
    }

    /// A capture more than 24h away is a *different* setup on the same
    /// instrument, not this plan's fixture.
    #[test]
    fn a_capture_outside_the_window_does_not_match() {
        let cells = vec![cell(
            "aud-cad-h1-2026-07-24-normal-news-off",
            "AUD_CAD",
            "h1",
            "2026-07-24T09:00:00Z",
        )];
        // Δ ≈ 48h.
        let status = status_for(&cells, "AUD_CAD", "h1", Some("2026-07-22T09:12:10Z"));
        assert_eq!(status, Status::None);
        assert_eq!(status.label(), "no fixture");
    }

    /// With two qualifying captures, the NEAREST one wins and only its cells are
    /// counted — the far one must not inflate the count.
    #[test]
    fn the_nearest_capture_wins_and_the_other_is_not_counted() {
        let mut cells = vec![
            cell(
                "aud-cad-h1-2026-07-22-normal-news-off",
                "AUD_CAD",
                "h1",
                "2026-07-22T10:00:00Z",
            ),
            cell(
                "aud-cad-h1-2026-07-22-normal-news-on",
                "AUD_CAD",
                "h1",
                "2026-07-22T10:00:00Z",
            ),
        ];
        // A second, further-away capture also inside the 24h window.
        cells.push(cell(
            "aud-cad-h1-2026-07-23-normal-news-off",
            "AUD_CAD",
            "h1",
            "2026-07-23T04:00:00Z",
        ));
        let status = status_for(&cells, "AUD_CAD", "h1", Some("2026-07-22T09:12:10Z"));
        assert_eq!(
            status,
            Status::Saved {
                base: "aud-cad-h1-2026-07-22".into(),
                cells: 2
            },
            "the nearer capture's 2 cells, not all 3"
        );
    }

    /// The corpus holds the same asset under two broker spellings (`NZD/CHF`
    /// and `NZD_CHF`). A raw string compare reports "no fixture" for whichever
    /// spelling the plan doesn't use — this is the bug the canonicalisation
    /// exists to prevent.
    #[test]
    fn instrument_matches_across_broker_spellings() {
        let cells = vec![cell(
            "nzd-chf-h1-2026-06-19-normal-news-off",
            "NZD/CHF",
            "h1",
            "2026-06-19T00:00:00Z",
        )];
        let status = status_for(&cells, "NZD_CHF", "h1", Some("2026-06-19T01:00:00Z"));
        assert!(
            status.is_saved(),
            "OANDA-spelled plan must match the TradeNation-spelled fixture: {status:?}"
        );
    }

    /// A different instrument on the same timeframe and day must not match.
    #[test]
    fn a_different_instrument_does_not_match() {
        let cells = vec![cell(
            "eur-usd-h1-2026-07-22-normal-news-off",
            "EUR_USD",
            "h1",
            "2026-07-22T10:00:00Z",
        )];
        let status = status_for(&cells, "AUD_CAD", "h1", Some("2026-07-22T09:12:10Z"));
        assert_eq!(status, Status::None);
    }

    /// Same instrument, same day, different timeframe — a distinct setup.
    #[test]
    fn a_different_granularity_does_not_match() {
        let cells = vec![cell(
            "eur-cad-h4-2026-07-21-normal-news-off",
            "EUR/CAD",
            "h4",
            "2026-07-21T10:00:00Z",
        )];
        let status = status_for(&cells, "EUR_CAD", "h1", Some("2026-07-21T10:30:00Z"));
        assert_eq!(
            status,
            Status::None,
            "h1 plan must not claim the h4 capture"
        );
    }

    /// Without an arm time the plan can't be placed on the timeline, so it
    /// matches nothing rather than claiming the first same-instrument capture.
    #[test]
    fn a_plan_with_no_armed_at_matches_nothing() {
        let cells = vec![cell(
            "aud-cad-h1-2026-07-22-normal-news-off",
            "AUD_CAD",
            "h1",
            "2026-07-22T10:00:00Z",
        )];
        assert_eq!(status_for(&cells, "AUD_CAD", "h1", None), Status::None);
        assert_eq!(
            status_for(&cells, "AUD_CAD", "h1", Some("not-a-timestamp")),
            Status::None
        );
    }

    /// An empty or missing corpus directory is a normal state, not an error.
    #[test]
    fn scanning_a_missing_dir_yields_an_empty_corpus() {
        let cells = scan(Path::new("/nonexistent/replay-fixtures"));
        assert!(cells.is_empty());
        assert_eq!(
            status_for(&cells, "AUD_CAD", "h1", Some("2026-07-22T09:12:10Z")),
            Status::None
        );
    }

    /// A cell whose meta carries fields this reader doesn't know must still
    /// load — the capture side owns that schema and adds to it.
    #[test]
    fn unknown_meta_fields_do_not_break_the_read() {
        let json = r#"{
            "instrument": "AUD_CAD",
            "granularity": "h1",
            "source": "oanda",
            "start": "2026-07-22T10:00:00Z",
            "end": "2026-07-24T06:00:00Z",
            "message": "a note",
            "arm": {"entry_rule": "normal", "journal_ref": null}
        }"#;
        let meta: CellMeta = serde_json::from_str(json).expect("tolerates extra fields");
        assert_eq!(meta.instrument, "AUD_CAD");
        assert_eq!(meta.granularity, "h1");
    }

    /// Scanning a real directory tree: two cells of one capture are found and
    /// grouped, and a directory without a `meta.json` is ignored.
    #[test]
    fn scan_reads_cells_and_skips_non_fixtures() {
        let tmp = std::env::temp_dir().join(format!("journal-fixture-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let meta = |instr: &str| {
            format!(
                r#"{{"instrument":"{instr}","granularity":"h1","source":"oanda",
                    "start":"2026-07-22T10:00:00Z","end":"2026-07-24T06:00:00Z"}}"#
            )
        };
        for name in [
            "aud-cad-h1-2026-07-22-normal-news-off",
            "aud-cad-h1-2026-07-22-normal-news-on",
        ] {
            let d = tmp.join(name);
            std::fs::create_dir_all(&d).expect("create cell dir");
            std::fs::write(d.join("meta.json"), meta("AUD_CAD")).expect("write meta");
        }
        // A directory that is not a fixture (no meta.json) must be skipped.
        std::fs::create_dir_all(tmp.join("not-a-fixture")).expect("create stray dir");

        let cells = scan(&tmp);
        assert_eq!(cells.len(), 2, "only the two real cells: {cells:?}");
        let status = status_for(&cells, "AUD_CAD", "h1", Some("2026-07-22T09:12:10Z"));
        assert_eq!(
            status,
            Status::Saved {
                base: "aud-cad-h1-2026-07-22".into(),
                cells: 2
            }
        );
        std::fs::remove_dir_all(&tmp).ok();
    }
}
