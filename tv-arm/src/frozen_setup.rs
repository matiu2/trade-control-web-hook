//! `--spec-out` / `--spec-in` / `--spec-url`: arm a setup again without
//! TradingView.
//!
//! The operator confirms a pattern on the chart **once**, writes a frozen setup,
//! and every later arm of that setup reads the file. No tv-mcp, no chart, no
//! risk that a rewound chart or a stale drawing hands back a different pattern
//! than the one that was confirmed.
//!
//! ## Two doors, one arm
//!
//! `--spec-in` reads the bytes from a file; `--spec-url` fetches them over
//! HTTP, pointed at local-chart's `GET /arm-setup`, which emits exactly this
//! struct. Both funnel through [`FrozenSetup::parse`] and then the same
//! `setup_from_frozen` in `pipeline`, so every restriction below holds
//! identically for either — a spec-url arm is a spec-in arm that skipped the
//! download step, not a second code path with its own rules.
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
    /// A **static trade** — a drawn position tool as absolute entry/SL/TP
    /// prices, rather than a pattern to derive a trade from.
    ///
    /// FROZEN, and it belongs in the frozen half without ambiguity: these
    /// three numbers ARE the operator's decision, in exactly the way
    /// [`PlanGeometry`] is for a pattern. Re-deriving them would be
    /// re-deciding the trade.
    ///
    /// `None` is the ordinary pattern spec, and is what every spec written
    /// before this field existed parses as. See [`crate::frozen_position`]
    /// for why a local-chart position can be frozen when a TradingView one
    /// cannot, and why the position-entry refusal narrows rather than
    /// disappears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<crate::frozen_position::FrozenPosition>,
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
            // `capture` is the LIVE-CHART freeze (`--spec-out`), whose
            // position tool is TradingView's: its SL/TP are tick offsets,
            // not prices, and converting them here would freeze numbers
            // derived from a `tick_size` that a later re-arm may read
            // differently from the catalog. Static trades reach a spec by
            // being WRITTEN as absolute prices by a producer that has them
            // (local-chart's `GET /arm-setup`), never by being captured
            // off a TradingView chart. See `crate::frozen_position`.
            position: None,
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
        Self::parse(&text, &path.display().to_string())
    }

    /// Parse a spec from its text, whatever produced it — a file on disk
    /// (`load`) or an HTTP body (`--spec-url`).
    ///
    /// Both paths go through here so the version gate cannot be enforced on one
    /// and forgotten on the other. `source` names where the bytes came from and
    /// appears in every error: a path for a file, a URL for a fetch. It is
    /// purely for the operator's benefit — nothing branches on it.
    pub fn parse(text: &str, source: &str) -> Result<Self> {
        let setup: Self =
            serde_json::from_str(text).wrap_err_with(|| format!("parse frozen setup {source}"))?;
        if setup.version > SPEC_VERSION {
            return Err(eyre!(
                "frozen setup {} is version {} but this tv-arm understands up to {}; \
                 upgrade tv-arm rather than arming from a spec it can't read",
                source,
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
        // Every OPTIONAL field must be Some here, or `skip_serializing_if`
        // omits it and this guard goes blind to it — which is exactly what
        // happened when `position` was added: the test passed against a
        // fixture whose `position` was None, so the new field slipped past
        // the one check meant to catch new fields.
        let full = FrozenSetup {
            position: Some(crate::frozen_position::FrozenPosition {
                direction: crate::frozen_position::FrozenDirection::Long,
                entry: 1.1000,
                stop_loss: 1.0950,
                take_profit: 1.1200,
            }),
            ..setup()
        };
        assert!(
            full.start.is_some() && full.note.is_some() && full.tv_arm_version.is_some(),
            "the fixture must populate every Option, or skip_serializing_if hides it"
        );
        let json = serde_json::to_value(&full).expect("serialize");
        let obj = json.as_object().expect("an object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "chart_symbol",
                "geom",
                "note",
                "position",
                "resolution",
                "start",
                "tv_arm_version",
                "version",
            ],
            "a field was added or removed — if added, decide FREEZE vs RE-READ \
             (see the module doc) before updating this list"
        );
    }

    /// A pattern spec must serialize WITHOUT a `position` key, not with a
    /// null one. Producers (local-chart included) are `deny_unknown_fields`
    /// in both directions, and a `"position": null` would also change the
    /// bytes of every spec written before this field existed.
    #[test]
    fn a_pattern_spec_omits_the_position_key_entirely() {
        let json = serde_json::to_value(setup()).expect("serialize");
        assert!(
            !json
                .as_object()
                .expect("an object")
                .contains_key("position"),
            "a pattern spec grew a null position key: {json}"
        );
    }

    /// A spec written before `position` existed must still parse — the
    /// field is `#[serde(default)]`, and every corpus fixture on disk
    /// predates it.
    #[test]
    fn a_spec_without_a_position_key_still_parses() {
        let json = r#"{"version":1,"geom":{},"resolution":"60",
                       "chart_symbol":"OANDA:EURUSD"}"#;
        let got = FrozenSetup::parse(json, "test").expect("an old spec parses");
        assert_eq!(got.position, None);
    }

    /// The operator's real local-chart export, verbatim, parses into the
    /// static trade it describes.
    #[test]
    fn the_real_local_chart_static_trade_export_parses() {
        let json = r#"{
            "version": 1,
            "geom": { "trade_expiry_epoch": 1790424000 },
            "resolution": "60",
            "chart_symbol": "TRADENATION:ESPIXEUR",
            "position": { "direction": "long", "entry": 19670.6,
                          "stop_loss": 19637.3, "take_profit": 19910.3 }
        }"#;
        let got = FrozenSetup::parse(json, "arm-setup").expect("parses");
        let pos = got.position.expect("a static trade");
        assert_eq!(pos.entry, 19670.6);
        assert_eq!(pos.stop_loss, 19637.3);
        assert_eq!(pos.take_profit, 19910.3);
        assert_eq!(got.geom.trade_expiry_epoch, Some(1790424000));
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

    // ------------------------------------------------ parsing, source-agnostic

    /// `parse` is the single seam both `load` (a file) and `--spec-url` (an
    /// HTTP body) go through, so the version gate cannot be enforced on one
    /// path and forgotten on the other.
    #[test]
    fn parse_accepts_a_current_version_spec() {
        let json = serde_json::to_string(&setup()).expect("serialize");
        let back = FrozenSetup::parse(&json, "test source").expect("parses");
        assert_eq!(back, setup());
    }

    #[test]
    fn parse_rejects_a_future_version_naming_the_source() {
        let mut s = setup();
        s.version = SPEC_VERSION + 1;
        let json = serde_json::to_string(&s).expect("serialize");
        let err = FrozenSetup::parse(&json, "http://127.0.0.1:8790/arm-setup")
            .expect_err("must refuse a spec it cannot read")
            .to_string();
        assert!(
            err.contains("http://127.0.0.1:8790/arm-setup"),
            "err = {err}"
        );
        assert!(err.contains("upgrade tv-arm"), "err = {err}");
    }

    /// local-chart answers a chart that is missing a required role with a
    /// structured 422 body. That is JSON, but it is not a `FrozenSetup` — the
    /// parse error must name the source so the operator knows what to fix.
    #[test]
    fn parse_rejects_a_non_spec_json_body() {
        let body = r#"{"error":"missing required roles","missing":["neckline"]}"#;
        let err = FrozenSetup::parse(body, "http://127.0.0.1:8790/arm-setup")
            .expect_err("must fail")
            .to_string();
        assert!(err.contains("parse frozen setup"), "err = {err}");
        assert!(err.contains("127.0.0.1:8790"), "err = {err}");
    }

    /// `deny_unknown_fields` is what catches a stray or misspelled key coming
    /// from another producer; prove it still fires through `parse`.
    #[test]
    fn parse_rejects_an_unknown_field() {
        let mut v: serde_json::Value = serde_json::to_value(setup()).expect("to value");
        v["surprise"] = serde_json::json!("extra");
        let err = FrozenSetup::parse(&v.to_string(), "test source")
            .expect_err("unknown fields must be refused")
            .to_string();
        assert!(err.contains("parse frozen setup"), "err = {err}");
    }
}
