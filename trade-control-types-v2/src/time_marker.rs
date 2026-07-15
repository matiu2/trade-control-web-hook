//! [`TimeMarker`] — a single wall-clock timestamp, the third geometry kind.
//!
//! The time sibling of [`Line`](crate::Line) (2 anchors, price projection) and
//! [`PriceLevel`](crate::PriceLevel) (1 price, no projection). A `TimeMarker` is 1
//! *time* and no price at all: its "cross" is "has the bar reached this time?"
//! (`candle.time >= at_epoch`) — the v1 `TimeReached` arm. Added in 4d.
//!
//! # Which names are markers
//!
//! Today just [`Expiry`](crate::Expiry) — the trade-expiry cutoff. (v1 also drives
//! `pause`/`resume` and `news-start`/`news-end` off `TimeReached`; those need a
//! news-window / pause state concept engine-v2's fact blackboard doesn't model
//! yet, so they're a later slice — 4d ships only the expiry marker + its rule, the
//! one time-based retirement that reuses 4c's `Effect::Invalidate` machinery
//! wholesale.)
//!
//! # Wire format
//!
//! `TimeMarker` serializes as `{ name, at_epoch }` — `at_epoch` a Unix epoch in
//! seconds, matching v1's `LinePoint.at_epoch` / `Trigger::TimeReached`.
//! `TradePlan.markers` is `#[serde(default)]` so a plan with no expiry — and any
//! pre-4d plan — deserializes with an empty vec.

use serde::{Deserialize, Serialize};

/// One named wall-clock marker — a time after which something happens (today: the
/// plan expires).
///
/// Unlike a [`Line`](crate::Line) or [`PriceLevel`](crate::PriceLevel) it has no
/// price: the "cross" is purely temporal (`candle.time >= at_epoch`). Today:
/// `Expiry` (the trade-expiry cutoff).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeMarker {
    /// The marker's name, e.g. `"expiry"`. Facts are keyed by this, the same name
    /// axis a line or level uses — only the geometry (a time, not a price) differs.
    pub name: String,
    /// The marker time as a **Unix epoch in seconds** (matching v1's
    /// `LinePoint.at_epoch`). A bar reaches the marker when
    /// `candle.time.timestamp() >= at_epoch`.
    pub at_epoch: i64,
}

#[cfg(test)]
mod tests {
    use crate::{Expiry, LineName, TimeMarker, TradePlan};
    use trade_control_core::broker::Granularity;
    use trade_control_core::intent::Direction;

    /// A minimal markers-only plan for the lookup/roundtrip tests.
    fn plan_with_markers(markers: Vec<TimeMarker>) -> TradePlan {
        TradePlan {
            trade_id: "t".into(),
            instrument: "EUR_USD".into(),
            direction: Direction::Short,
            granularity: Granularity::H1,
            lines: Vec::new(),
            levels: Vec::new(),
            markers,
            pause_windows: Vec::new(),
            rules: Vec::new(),
            cross_buffer_pct: 0.0,
            retest_atr_step: 0.0,
        }
    }

    /// `marker_typed::<L>()` resolves the marker named `L::NAME`, misses for one the
    /// plan doesn't carry — the marker mirror of `line_typed` / `level_typed`.
    #[test]
    fn marker_typed_resolves() {
        let plan = plan_with_markers(vec![TimeMarker {
            name: Expiry::NAME.into(),
            at_epoch: 1_780_000_000,
        }]);

        assert_eq!(
            plan.marker_typed::<Expiry>().map(|m| m.at_epoch),
            Some(1_780_000_000),
            "Expiry resolves",
        );
        // Runtime-name lookup agrees.
        assert_eq!(
            plan.marker(Expiry::NAME).map(|m| m.at_epoch),
            Some(1_780_000_000),
            "marker(name) agrees with marker_typed",
        );
        // An empty-markers plan is a clean miss.
        assert!(
            plan_with_markers(Vec::new())
                .marker_typed::<Expiry>()
                .is_none(),
            "no markers ⇒ None",
        );
    }

    /// The `markers` field round-trips through serde, and its `#[serde(default)]`
    /// lets a plan JSON that predates the field deserialize with an empty vec — the
    /// wire-compat guard for the 4d split.
    #[test]
    fn markers_wire_roundtrip_and_default() {
        let plan = plan_with_markers(vec![TimeMarker {
            name: Expiry::NAME.into(),
            at_epoch: 1_780_000_000,
        }]);
        let json = serde_json::to_string(&plan).expect("serialize");
        assert!(json.contains("\"expiry\""), "marker name on the wire");
        let back: TradePlan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            back.marker_typed::<Expiry>().map(|m| m.at_epoch),
            Some(1_780_000_000),
            "marker survives the round-trip",
        );

        // A pre-4d plan JSON with NO `markers` key deserializes with an empty vec.
        let bare_json = serde_json::to_string(&plan_with_markers(Vec::new())).expect("serialize");
        let legacy = bare_json.replace("\"markers\":[],", "");
        assert!(
            !legacy.contains("\"markers\""),
            "markers key removed for the test"
        );
        let restored: TradePlan = serde_json::from_str(&legacy).expect("deserialize legacy");
        assert!(
            restored.markers.is_empty(),
            "missing markers key ⇒ empty vec via serde(default)",
        );
    }
}
