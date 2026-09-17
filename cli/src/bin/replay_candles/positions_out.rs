//! Emit the replayed positions as JSON, for a chart layer to draw.
//!
//! ## Why the simulator does not draw
//!
//! Drawing is not this binary's job. `replay-candles` simulates a plan over a
//! candle window and reports what happened; *where* that gets painted is
//! somebody else's concern. The existing `--annotate` path breaks that rule —
//! it reaches straight out to a TradingView bridge — and the cost shows: the
//! drawing code is shaped around TradingView's position tool, its re-run
//! hygiene is a sidecar file of TradingView entity-ids, and adding a second
//! chart meant teaching the simulator about a second chart.
//!
//! This module is the seam that fixes that. `--positions <path>` writes
//! **what happened**; a consumer decides whether to draw it and where. The
//! payload is the same [`FireResult`] set `--annotate` draws, so nothing is
//! recomputed and the two cannot disagree.
//!
//! ⚠️ The TradingView path (`super::annotate`) is deliberately untouched. It
//! stays until the new one has replaced it in practice, then gets **deleted** —
//! not refactored into a backend of this one. See `TODO-positions-emit.md`.
//!
//! ## Its own wire types
//!
//! The structs here mirror [`FireResult`] rather than deriving `Serialize`
//! onto it. `FireResult` is a simulator internal that changes when the
//! simulator changes; this file is a contract with another program. Renaming a
//! field there should be a compile error here, not a silently-altered wire
//! format a consumer parses into the wrong shape.

use std::path::Path;

use color_eyre::eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use trade_control_core::intent::Direction;

use super::report::{FillKind, FireResult};

/// Bumped when a consumer would parse an older file wrongly. A consumer must
/// refuse a `version` it does not know rather than guess at the fields.
pub const POSITIONS_VERSION: u32 = 1;

/// One replayed position: absolute price levels and the bars they span.
///
/// Prices are **absolute**, deliberately. TradingView's own position tool
/// stores its stop and target as tick offsets from the entry, which needs the
/// instrument's tick size to interpret — a conversion that belongs at the
/// drawing end, if a backend needs it at all. Absolute prices are what the
/// replay actually computed, so this format does not make a consumer undo an
/// encoding to recover them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionRow {
    /// `long` or `short` — lowercase, matching the vocabulary the CLI flags
    /// and local-chart's drawing types already use.
    pub direction: String,
    /// Open-time of the bar the entry filled on (UNIX seconds, UTC). For a
    /// not-taken outcome this is the *fire* bar — where the order would have
    /// been placed.
    pub fill_at: i64,
    /// Right-edge bar (UNIX seconds, UTC): the exit bar, or the last replayed
    /// bar for a position still open at the window's end.
    pub until: i64,
    /// The placed entry level the fill happened at (or the intended level, for
    /// a not-taken outcome).
    pub entry_price: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    /// Actual exit price for a closed trade; `None` while open or not taken.
    pub exit_price: Option<f64>,
    /// How it resolved: `open`, `sl`, `tp`, `reversal`, `expiry`,
    /// `invalidation`, `no-fill`, `declined`, `gate-blocked`.
    pub outcome: String,
    /// Whether the position was actually **taken**. A consumer draws these
    /// differently (the not-taken ones are only an *intended* bracket), and
    /// deriving that from `outcome` would mean every consumer re-encoding the
    /// taken/not-taken split this binary already knows.
    pub taken: bool,
}

/// The `--positions` document: one chart, and the positions replayed on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionsFile {
    pub version: u32,
    /// The instrument as the replay was given it — NOT resolved to any
    /// broker's convention. Resolution is the consumer's job: local-chart
    /// wants a bare OANDA symbol, TradingView wants an exchange-qualified one,
    /// and baking either choice in here would make this file backend-specific,
    /// which is the whole thing this seam exists to avoid.
    pub instrument: String,
    /// Bar size as the replay was given it (`1h`, `4h`, `1d`…).
    pub granularity: String,
    pub positions: Vec<PositionRow>,
}

/// The wire name for an outcome. Kept as an explicit match, so adding a
/// [`FillKind`] variant is a compile error here rather than a silent
/// `"unknown"` a consumer would have to guess at.
fn outcome_name(kind: FillKind) -> &'static str {
    match kind {
        FillKind::Open => "open",
        FillKind::StoppedOut => "sl",
        FillKind::TookProfit => "tp",
        FillKind::ClosedOnReversal => "reversal",
        FillKind::ClosedAtExpiry => "expiry",
        FillKind::ClosedOnInvalidation => "invalidation",
        FillKind::NeverFilled => "no-fill",
        FillKind::Declined => "declined",
        FillKind::GateBlocked => "gate-blocked",
    }
}

impl PositionRow {
    /// Project one resolved fire onto the wire.
    pub fn from_fire(fire: &FireResult) -> Self {
        Self {
            direction: match fire.direction {
                Direction::Long => "long",
                Direction::Short => "short",
            }
            .to_string(),
            fill_at: fire.fill_at.timestamp(),
            until: fire.until.timestamp(),
            entry_price: fire.entry_price,
            stop_loss: fire.stop_loss,
            take_profit: fire.take_profit,
            exit_price: fire.exit_price,
            outcome: outcome_name(fire.kind).to_string(),
            taken: fire.kind.is_taken(),
        }
    }
}

impl PositionsFile {
    pub fn new(instrument: &str, granularity: &str, fires: &[FireResult]) -> Self {
        Self {
            version: POSITIONS_VERSION,
            instrument: instrument.to_string(),
            granularity: granularity.to_string(),
            positions: fires.iter().map(PositionRow::from_fire).collect(),
        }
    }

    /// Write to `path`, creating parent directories as needed.
    ///
    /// Pretty-printed: these files are read by a person as often as by a
    /// program when a drawing looks wrong, and the size is trivial (a replay
    /// yields a handful of positions).
    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .wrap_err_with(|| format!("creating {} for --positions", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(self).wrap_err("serialising positions")?;
        std::fs::write(path, json)
            .wrap_err_with(|| format!("writing positions to {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).expect("a valid test timestamp")
    }

    fn fire(kind: FillKind, direction: Direction) -> FireResult {
        FireResult {
            direction,
            fill_at: at(1_700_000_000),
            until: at(1_700_086_400),
            entry_price: 1.2000,
            stop_loss: 1.1950,
            take_profit: 1.2100,
            exit_price: Some(1.2100),
            kind,
        }
    }

    /// The levels are what a chart draws. If any of them shifted in transit,
    /// the drawn bracket would be a different trade from the one replayed.
    #[test]
    fn the_absolute_levels_cross_the_wire_untouched() {
        let row = PositionRow::from_fire(&fire(FillKind::TookProfit, Direction::Long));
        assert_eq!(row.entry_price, 1.2000);
        assert_eq!(row.stop_loss, 1.1950);
        assert_eq!(row.take_profit, 1.2100);
        assert_eq!(row.exit_price, Some(1.2100));
    }

    #[test]
    fn direction_is_the_lowercase_word_the_cli_and_local_chart_both_use() {
        let long = PositionRow::from_fire(&fire(FillKind::Open, Direction::Long));
        let short = PositionRow::from_fire(&fire(FillKind::Open, Direction::Short));
        assert_eq!(long.direction, "long");
        assert_eq!(short.direction, "short");
    }

    /// A consumer draws taken and not-taken positions differently, so the
    /// split must be carried explicitly rather than re-derived from `outcome`
    /// by every consumer that reads the file.
    #[test]
    fn the_taken_flag_matches_the_outcome_it_came_from() {
        for (kind, want) in [
            (FillKind::Open, true),
            (FillKind::StoppedOut, true),
            (FillKind::TookProfit, true),
            (FillKind::ClosedOnReversal, true),
            (FillKind::ClosedAtExpiry, true),
            (FillKind::ClosedOnInvalidation, true),
            (FillKind::NeverFilled, false),
            (FillKind::Declined, false),
            (FillKind::GateBlocked, false),
        ] {
            let row = PositionRow::from_fire(&fire(kind, Direction::Long));
            assert_eq!(row.taken, want, "{kind:?} taken-ness");
        }
    }

    /// Every outcome needs a distinct wire name: two kinds sharing one would
    /// erase a distinction the replay worked to establish (the
    /// expiry-vs-invalidation split, for one, which was a real bug).
    #[test]
    fn every_outcome_has_its_own_distinct_wire_name() {
        let names: Vec<&str> = [
            FillKind::Open,
            FillKind::StoppedOut,
            FillKind::TookProfit,
            FillKind::ClosedOnReversal,
            FillKind::ClosedAtExpiry,
            FillKind::ClosedOnInvalidation,
            FillKind::NeverFilled,
            FillKind::Declined,
            FillKind::GateBlocked,
        ]
        .into_iter()
        .map(outcome_name)
        .collect();

        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            names.len(),
            "outcome names collide: {names:?}"
        );
    }

    /// The instrument must cross UNRESOLVED. Baking in one backend's
    /// convention here would make the file backend-specific — local-chart
    /// wants `EUR_USD`, TradingView wants `OANDA:EURUSD`.
    #[test]
    fn the_instrument_is_carried_verbatim_not_resolved_to_a_broker() {
        let doc = PositionsFile::new("eur/cad", "1h", &[]);
        assert_eq!(doc.instrument, "eur/cad", "exactly as the replay got it");
        assert_eq!(doc.granularity, "1h");
    }

    #[test]
    fn the_document_round_trips_through_json() {
        let doc = PositionsFile::new(
            "EUR_USD",
            "4h",
            &[
                fire(FillKind::TookProfit, Direction::Long),
                fire(FillKind::NeverFilled, Direction::Short),
            ],
        );
        let json = serde_json::to_string(&doc).expect("serialises");
        let back: PositionsFile = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back, doc);
        assert_eq!(back.positions.len(), 2);
        assert_eq!(back.version, POSITIONS_VERSION);
    }

    #[test]
    fn writing_creates_missing_parent_directories() {
        let dir = std::env::temp_dir().join(format!("rc-positions-{}", std::process::id()));
        let path = dir.join("nested").join("positions.json");
        let doc = PositionsFile::new("EUR_USD", "1h", &[fire(FillKind::Open, Direction::Long)]);

        doc.write(&path).expect("writes through missing dirs");
        let text = std::fs::read_to_string(&path).expect("file is there");
        let back: PositionsFile = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(back, doc);

        std::fs::remove_dir_all(&dir).ok();
    }
}
