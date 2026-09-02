//! `--spec-out` / `--spec-in`: arm a setup again without TradingView.
//!
//! The operator confirms a pattern on the chart **once**, writes a frozen setup,
//! and every later arm of that setup reads the file. No tv-mcp, no chart, no
//! risk that a rewound chart or a stale drawing hands back a different pattern
//! than the one that was confirmed.
//!
//! ## What is frozen, and what is deliberately re-read
//!
//! Getting this split wrong bakes wrong numbers into 291 trades, so it is stated
//! explicitly rather than left to whatever happened to be convenient.
//!
//! **Frozen** — it *is* the setup, and re-deriving it would be re-deciding it:
//!
//! - the drawn geometry ([`PlanGeometry`]);
//! - the **granularity** the pattern was read at — load-bearing, see below;
//! - the broker-qualified chart symbol, so the same feed is used;
//! - the arm cursor (`--start`), which pins reproducibility.
//!
//! **Re-read on every arm** — a frozen copy would be *stale*, not reproducible:
//!
//! - **broker spread** (M/W entry sizing) — a frozen spread mis-sizes the entry;
//! - **live mid** (the `--pull-back` anchor) — it *is* "price at arm time";
//! - **calendar / news windows** — a function of the new arm time, not the old
//!   one. This is why a spec-in arm is **not** bit-reproducible across days, and
//!   why the tier-2 baseline diff labels news-ON rows `[calendar]`: they can
//!   move because the calendar moved. That is correct behaviour, not drift.
//! - **instrument-lookup pip/tick** — a pure local catalog lookup, free, and a
//!   frozen copy would silently outlive a catalog correction.
//!
//! ## Granularity is the sharp edge
//!
//! It is **not** on [`PlanGeometry`] (a chart resolution isn't geometry), so it
//! has to be carried here or it is lost. It feeds `TrendlineCross.bar_seconds`,
//! and trendline prices interpolate in **bar-index** space — so the same
//! neckline anchors read at H1 versus H4 produce *different prices at the same
//! instant*. Measured on identical anchors: **1.116667 (H1) vs 1.123333 (H4)**,
//! about 67 pips apart, with no error raised anywhere.
//!
//! Today `run` takes it from the live chart (`state.resolution`). A re-arm off a
//! chart left on another timeframe would therefore reprice the whole neckline
//! and produce a plausible, wrong plan. Frozen here, that can't happen.
//!
//! ## Position tools are refused, not silently ignored
//!
//! `--market-entry` / `--stop-entry` / `--limit-entry` read the drawn position
//! tool, whose SL/TP are TradingView **drawing properties** with no frozen
//! equivalent. A frozen arm has no `Roles` at all, so those flags are rejected
//! up front with a message saying why — rather than arming some other trade.

use std::path::Path;

use color_eyre::eyre::{Context, Result, eyre};
use serde::{Deserialize, Serialize};

use crate::plan_geometry::PlanGeometry;

/// The chart-derived facts worth freezing. See the module doc for what is
/// deliberately absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenSetup {
    /// Schema version. Bumped when a field's *meaning* changes, so an old file
    /// fails loudly instead of being reinterpreted.
    pub version: u32,
    /// The drawn setup, as plain data.
    pub geom: PlanGeometry,
    /// The chart resolution the pattern was read at (`60`, `240`, `D`).
    ///
    /// **Load-bearing** — see the module doc. Not derivable from `geom`: the
    /// anchors are `(epoch, price)` pairs, and the bar SIZE is not recoverable
    /// from them.
    pub resolution: String,
    /// Broker-**qualified** TradingView symbol (`TRADENATION:EURUSD`).
    ///
    /// Qualified on purpose: a bare TradingView symbol silently resolves to the
    /// OANDA feed, so an unqualified capture would re-arm against a different
    /// price feed and look entirely plausible.
    pub chart_symbol: String,
    /// The journaling cursor (`--start`) this setup was read at, as an epoch.
    ///
    /// `None` means the setup was captured live with no cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
    /// Free-text note from the operator (`--spec-note`), e.g. the journal page
    /// this setup came from. Never read by the arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The tv-arm version that captured it, for provenance when a re-arm
    /// disagrees with the original.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tv_arm_version: Option<String>,
}

/// The current [`FrozenSetup::version`].
pub const SPEC_VERSION: u32 = 1;

impl FrozenSetup {
    /// Capture from a live arm's inputs.
    pub fn capture(
        geom: PlanGeometry,
        resolution: String,
        chart_symbol: String,
        start: Option<i64>,
        note: Option<String>,
    ) -> Self {
        Self {
            version: SPEC_VERSION,
            geom,
            resolution,
            chart_symbol,
            start,
            note,
            tv_arm_version: Some(env!("GIT_VERSION").to_string()),
        }
    }

    /// Write as pretty JSON, with a trailing newline so the file is diffable and
    /// plays nicely with line-oriented tools.
    ///
    /// Creates the parent directory if it is missing. The chart read is the
    /// expensive, human-paced part of a capture, and losing it to a bare
    /// `No such file or directory` — after the operator has already confirmed
    /// the pattern — costs a whole chart session to redo. A corpus directory
    /// that does not exist yet is the normal state of a fresh checkout, not an
    /// error worth failing a capture over.
    pub fn write(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).wrap_err("serialize frozen setup")?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("create fixtures directory {}", parent.display()))?;
        }
        std::fs::write(path, format!("{json}\n"))
            .wrap_err_with(|| format!("write frozen setup to {}", path.display()))
    }

    /// Load, rejecting a version this build doesn't understand.
    ///
    /// A future-versioned file is a **hard error**, not a best-effort parse: the
    /// whole point of the artifact is that it re-arms the same setup, and
    /// silently reinterpreting a field whose meaning changed is exactly the
    /// failure mode it exists to prevent.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("read frozen setup {}", path.display()))?;
        let setup: Self = serde_json::from_str(&text)
            .wrap_err_with(|| format!("parse frozen setup {}", path.display()))?;
        if setup.version > SPEC_VERSION {
            return Err(eyre!(
                "frozen setup {} is version {} but this tv-arm understands up to {}; \
                 upgrade tv-arm rather than arming from a spec it can't read",
                path.display(),
                setup.version,
                SPEC_VERSION,
            ));
        }
        Ok(setup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan_geometry::{Anchor, Line, MwPath};

    fn geom() -> PlanGeometry {
        PlanGeometry {
            neckline: Some(Line {
                a: Anchor::new(1_750_000_000, 1.1000),
                b: Anchor::new(1_750_360_000, 1.1200),
            }),
            invalidation: Some(1.1500),
            stop_loss: None,
            fib_head_neckline: Some((1.0800, 1.1000)),
            trade_expiry_epoch: Some(1_750_600_000),
            prep_expiry_epochs: vec![("retest".into(), 1_750_500_000)],
            mw_path: None,
            sr_levels: vec![1.0950, 1.1250],
        }
    }

    fn setup() -> FrozenSetup {
        FrozenSetup::capture(
            geom(),
            "60".into(),
            "TRADENATION:EURUSD".into(),
            Some(1_750_400_000),
            Some("journal p.124".into()),
        )
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("frozen-{}-{}.json", std::process::id(), name))
    }

    /// The round trip must be **exact**. A spec that loses a field doesn't fail
    /// — it arms a different trade, quietly. Same failure shape that hit
    /// `PlanGeometry` three times (`runup_start`, `sr_levels`, `anchors`).
    #[test]
    fn a_frozen_setup_round_trips_exactly() {
        let s = setup();
        let path = tmp("roundtrip");
        s.write(&path).expect("write");
        let back = FrozenSetup::load(&path).expect("load");
        assert_eq!(s, back);
        std::fs::remove_file(&path).ok();
    }

    /// Every field must survive, checked by NAME against an explicit list.
    ///
    /// A plain round-trip can't catch a dropped field — if `capture` never set
    /// it and `load` never reads it, both sides agree on its absence and the
    /// test passes. This is the same guard `PlanGeometry` needed after the
    /// round-trip test missed `runup_start`.
    #[test]
    fn the_serialized_form_carries_every_field() {
        let json = serde_json::to_value(setup()).expect("serialize");
        let obj = json.as_object().expect("an object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "chart_symbol",
                "geom",
                "note",
                "resolution",
                "start",
                "tv_arm_version",
                "version",
            ],
            "a field was added or removed — if added, decide FREEZE vs RE-READ \
             (see the module doc) before updating this list"
        );
    }

    /// The resolution is not recoverable from the geometry, which is exactly why
    /// it is frozen. Anchors are `(epoch, price)`; the bar SIZE isn't in them.
    ///
    /// Losing it reprices the whole neckline — measured ~67 pips between H1 and
    /// H4 on identical anchors, with no error.
    #[test]
    fn the_resolution_is_frozen_because_geometry_cannot_supply_it() {
        let h1 = FrozenSetup::capture(geom(), "60".into(), "X:Y".into(), None, None);
        let h4 = FrozenSetup::capture(geom(), "240".into(), "X:Y".into(), None, None);
        assert_eq!(h1.geom, h4.geom, "identical geometry …");
        assert_ne!(h1.resolution, h4.resolution, "… different bar size");
        // So a spec that dropped `resolution` would make these two files equal,
        // and one of the two arms would be wrong.
        assert_ne!(h1, h4);
    }

    /// The chart symbol keeps its exchange prefix. A bare `EURUSD` silently
    /// resolves to the OANDA feed on TradingView, so a TradeNation capture that
    /// dropped it would re-arm against different price data.
    #[test]
    fn the_chart_symbol_stays_broker_qualified() {
        assert!(setup().chart_symbol.contains(':'));
    }

    /// An unknown key is a hard load error, not a silent ignore — serde's
    /// default would let a renamed field vanish and the spec would arm without
    /// it.
    #[test]
    fn an_unknown_key_is_rejected() {
        let mut json = serde_json::to_value(setup()).expect("serialize");
        json.as_object_mut()
            .expect("object")
            .insert("stray".into(), serde_json::json!(1));
        let text = serde_json::to_string(&json).expect("string");
        assert!(serde_json::from_str::<FrozenSetup>(&text).is_err());
    }

    /// A newer spec version is refused rather than reinterpreted.
    #[test]
    fn a_future_version_is_refused() {
        let mut s = setup();
        s.version = SPEC_VERSION + 1;
        let path = tmp("future");
        s.write(&path).expect("write");
        let err = FrozenSetup::load(&path)
            .expect_err("must refuse")
            .to_string();
        assert!(err.contains("understands up to"), "err = {err}");
        std::fs::remove_file(&path).ok();
    }

    /// An M/W setup freezes its path, anchor count included.
    #[test]
    fn an_mw_setup_freezes_its_path_and_anchor_count() {
        let mut g = geom();
        g.mw_path = Some(MwPath {
            runup_start: 1.1000,
            first_point: 1.1200,
            neckline: 1.1120,
            right_shoulder: Some(1.1190),
            anchors: 4,
        });
        let s = FrozenSetup::capture(g, "60".into(), "X:Y".into(), None, None);
        let path = tmp("mw");
        s.write(&path).expect("write");
        let back = FrozenSetup::load(&path).expect("load");
        assert_eq!(
            back.geom.mw_path.as_ref().map(|p| p.anchors),
            Some(4),
            "the anchor count must survive — without it a 5-anchor path re-arms \
             as a 4-anchor one"
        );
        assert_eq!(s, back);
        std::fs::remove_file(&path).ok();
    }

    /// A capture into a corpus directory that does not exist yet must create it
    /// rather than throw away the chart read. This is the reported failure:
    /// `write frozen setup to …/replay-fixtures/… : No such file or directory`,
    /// raised after the pattern had already been confirmed on the chart.
    #[test]
    fn writing_into_a_missing_corpus_directory_creates_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp
            .path()
            .join("replay-fixtures")
            .join("eur-cad-h1-2026-08-16.spec.json");
        assert!(!path.parent().expect("has parent").exists());

        setup()
            .write(&path)
            .expect("write must create the corpus dir");

        let back = FrozenSetup::load(&path).expect("reads back");
        assert_eq!(back, setup(), "the round trip must survive the mkdir");
    }

    /// A missing file is a clean error naming the path, not a panic.
    #[test]
    fn a_missing_spec_file_errors_cleanly() {
        let err = FrozenSetup::load(Path::new("/definitely/not/here.json"))
            .expect_err("must fail")
            .to_string();
        assert!(err.contains("read frozen setup"), "err = {err}");
    }
}
