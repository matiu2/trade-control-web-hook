//! [`PriceLevel`] — a single horizontal price, the second geometry kind.
//!
//! Split out of [`Line`](crate::Line) in 4c (see
//! `SCOPING-engine-v2-typed-geometry.md`). A [`Line`](crate::Line) is two anchors
//! crossed by **bar-index projection** (`line_price_at`); a `PriceLevel` is one
//! price with **no projection** — a "cross" is just `price ≷ level`. The two used
//! to share one `Line{a,b}` type, the caps expressed as a degenerate `a.price ==
//! b.price` trendline that still ran the full projection it never needed. Giving
//! the caps their own type makes the plan model match what `cross.rs` already
//! does differently for a level (the v1 `HorizontalCross`/`PriceValueCross` arm,
//! no `line_price_at`).
//!
//! # Which lines are levels
//!
//! `TooHigh` / `TooLow` — the invalidation / pcl-exhausted caps. They reuse the
//! same [`LineName`](crate::LineName) marker axis as a real line (a level *has* a
//! name, keyed the same way in facts); the split is about **geometry**, not the
//! name. `Neckline` stays a [`Line`](crate::Line).
//!
//! # Wire format
//!
//! `PriceLevel` serializes as `{ name, price }`. The name is still the marker's
//! [`NAME`](crate::LineName::NAME) string on the wire, exactly as a line's name
//! is. `TradePlan.levels` is `#[serde(default)]` so a plan with no caps — and any
//! pre-4c plan that predates the field — deserializes with an empty vec.

use serde::{Deserialize, Serialize};

/// One named horizontal price level — an invalidation / exhaustion cap.
///
/// Unlike a [`Line`](crate::Line) it has no anchors and no slope: its "cross" is a
/// direct `price ≷ level` test with no bar-index projection. Today: `TooHigh`
/// (short cap / iH&S ceiling) and `TooLow` (pcl-exhausted / iH&S floor).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceLevel {
    /// The level's name, e.g. `"too_high"`. Facts are keyed by this, exactly as a
    /// [`Line`](crate::Line)'s name is — the name axis is shared; only the
    /// geometry differs.
    pub name: String,
    /// The horizontal price. A cross is `candle` reaching this level in the rule's
    /// direction (no projection).
    pub price: f64,
}

#[cfg(test)]
mod tests {
    use crate::{LineName, PriceLevel, TooHigh, TooLow, TradePlan};
    use trade_control_core::broker::Granularity;
    use trade_control_core::intent::Direction;

    /// A minimal levels-only plan for the lookup/roundtrip tests.
    fn plan_with_levels(levels: Vec<PriceLevel>) -> TradePlan {
        TradePlan {
            trade_id: "t".into(),
            instrument: "EUR_USD".into(),
            direction: Direction::Short,
            granularity: Granularity::H1,
            lines: Vec::new(),
            levels,
            markers: Vec::new(),
            rules: Vec::new(),
            cross_buffer_pct: 0.0,
            retest_atr_step: 0.0,
        }
    }

    /// `level_typed::<L>()` resolves the level whose name is `L::NAME`, and misses
    /// (returns `None`) for a cap the plan doesn't carry — the level mirror of
    /// `line_typed`.
    #[test]
    fn level_typed_resolves() {
        let plan = plan_with_levels(vec![PriceLevel {
            name: TooHigh::NAME.into(),
            price: 1.2345,
        }]);

        let hit = plan.level_typed::<TooHigh>();
        assert_eq!(hit.map(|l| l.price), Some(1.2345), "TooHigh resolves");
        // A cap not in the plan is a clean miss, not a wrong hit.
        assert!(
            plan.level_typed::<TooLow>().is_none(),
            "TooLow absent ⇒ None"
        );
        // Runtime-name lookup agrees with the typed one.
        assert_eq!(
            plan.level(TooHigh::NAME).map(|l| l.price),
            Some(1.2345),
            "level(name) agrees with level_typed",
        );
    }

    /// The `levels` field round-trips through serde, and its `#[serde(default)]`
    /// lets a plan JSON that predates the field (no `levels` key) deserialize with
    /// an empty vec — the wire-compat guard for the 4c split.
    #[test]
    fn levels_wire_roundtrip_and_default() {
        // Explicit levels survive a serialize → deserialize round-trip.
        let plan = plan_with_levels(vec![
            PriceLevel {
                name: TooHigh::NAME.into(),
                price: 1.30,
            },
            PriceLevel {
                name: TooLow::NAME.into(),
                price: 1.20,
            },
        ]);
        let json = serde_json::to_string(&plan).expect("serialize");
        assert!(json.contains("\"too_high\""), "level name on the wire");
        let back: TradePlan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.levels.len(), 2, "both caps survive the round-trip");
        assert_eq!(back.level_typed::<TooLow>().map(|l| l.price), Some(1.20));

        // A pre-4c plan JSON with NO `levels` key deserializes with an empty vec
        // (the `#[serde(default)]` on the field). Build such a JSON by stripping
        // the field from the serialized form of a no-levels plan.
        let bare = plan_with_levels(Vec::new());
        let bare_json = serde_json::to_string(&bare).expect("serialize");
        // Remove the `"levels":[],` key to simulate an older plan that never had it.
        let legacy = bare_json.replace("\"levels\":[],", "");
        assert!(
            !legacy.contains("\"levels\""),
            "levels key removed for the test"
        );
        let restored: TradePlan = serde_json::from_str(&legacy).expect("deserialize legacy");
        assert!(
            restored.levels.is_empty(),
            "missing levels key ⇒ empty vec via serde(default)",
        );
    }
}
