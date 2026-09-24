//! A **static trade** carried in a frozen spec: the drawn position tool as
//! absolute entry / SL / TP prices.
//!
//! ## Why this can be frozen when the TradingView tool could not
//!
//! [`crate::frozen_setup`] used to refuse the position-entry flags outright,
//! and its reason was sound *for TradingView*: that tool's SL/TP are drawing
//! **properties** (`stopLevel` / `profitLevel`) expressed as **tick
//! offsets**, recoverable only by multiplying by the instrument's
//! `tick_size` at arm time — see [`crate::position_trade`]. There is
//! genuinely nothing in a frozen spec to recover them from.
//!
//! local-chart's position tool is a different shape. Its vendored library
//! declares `requiredAnchors: 3` and `getPositionInfo()` reads
//! `_anchors[0..2].price` **directly** — there are no tick-distance
//! properties at all, and all three levels are absolute prices the moment
//! they are drawn. So a frozen equivalent does exist; it simply had nowhere
//! to live until [`crate::frozen_setup::FrozenSetup::position`].
//!
//! **The refusal therefore narrows rather than disappears.** A spec WITHOUT
//! a `position` still refuses the entry flags, with the same message and for
//! the same reason. Only a spec that carries one is allowed through, because
//! only then are the prices actually present.
//!
//! ## These prices are used verbatim
//!
//! [`resolve_levels`] is deliberately NOT called on this path. Multiplying an
//! already-absolute price by `tick_size` would produce a plausible,
//! catastrophically wrong number (for the operator's ESPIX long: a 19670.6
//! entry against a 0.01 tick). The type exists partly to make that mistake
//! unrepresentable — it holds `PositionLevels`, not offsets.

use serde::{Deserialize, Serialize};

use crate::position_trade::PositionLevels;
use crate::roles::PositionDirection;

/// Which way the static trade goes.
///
/// Serialized as the lowercase word (`"long"` / `"short"`) that local-chart's
/// `GET /arm-setup` emits and that the operator sees in the JSON. Kept
/// separate from [`PositionDirection`] (which is keyed to TradingView's
/// `long_position` / `short_position` drawing types) so the wire format is
/// not hostage to a chart library's naming; [`FrozenPosition::direction`]
/// bridges them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrozenDirection {
    Long,
    Short,
}

impl FrozenDirection {
    fn to_position_direction(self) -> PositionDirection {
        match self {
            FrozenDirection::Long => PositionDirection::Long,
            FrozenDirection::Short => PositionDirection::Short,
        }
    }
}

/// A drawn static trade, frozen. Field-for-field the shape local-chart's
/// `GET /arm-setup` emits under `"position"`.
///
/// `deny_unknown_fields` for the same reason [`crate::frozen_setup`] uses it:
/// a misspelled or stray key is a producer bug, and failing loudly here is
/// strictly better than arming a trade with a level that silently defaulted.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenPosition {
    pub direction: FrozenDirection,
    /// Absolute entry price — **not** an offset. See the module doc.
    pub entry: f64,
    /// Absolute stop-loss price.
    pub stop_loss: f64,
    /// Absolute take-profit price.
    pub take_profit: f64,
}

impl FrozenPosition {
    /// The core direction this trade is in.
    pub fn direction(self) -> PositionDirection {
        self.direction.to_position_direction()
    }

    /// The three prices, ready to drop onto an enter intent.
    ///
    /// No `tick_size` argument, deliberately: there is no conversion to do,
    /// and a parameter would invite one. Contrast
    /// [`crate::position_trade::resolve_levels`], whose whole job is that
    /// conversion for the TradingView tool.
    pub fn levels(self) -> PositionLevels {
        PositionLevels {
            entry: self.entry,
            stop_loss: self.stop_loss,
            take_profit: self.take_profit,
        }
    }

    /// Reject a trade whose numbers cannot be traded, naming the offending
    /// value.
    ///
    /// Checked at parse time rather than at order-build time so the operator
    /// hears it while looking at the chart. Three classes, each of which
    /// would otherwise reach a **signed order**:
    ///
    /// - a non-finite or non-positive price (a NaN travels all the way down
    ///   and compares false against every guard on the way);
    /// - a stop on the wrong side of entry — that is not a stop, it is an
    ///   instant exit, and for a long it also inverts the risk calculation
    ///   that sizes the position;
    /// - a target on the wrong side of entry, i.e. a trade that can only win
    ///   by going the wrong way.
    ///
    /// Deliberately NOT checked: that SL and TP are any particular distance
    /// apart. A 0.2R trade is a bad trade, not an invalid one, and this
    /// refusing it would be the tool overruling the operator.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("entry", self.entry),
            ("stop_loss", self.stop_loss),
            ("take_profit", self.take_profit),
        ] {
            if !value.is_finite() {
                return Err(format!("position {name} is not a finite number ({value})"));
            }
            if value <= 0.0 {
                return Err(format!("position {name} must be positive, got {value}"));
            }
        }
        let (stop_ok, target_ok, side) = match self.direction {
            FrozenDirection::Long => (
                self.stop_loss < self.entry,
                self.take_profit > self.entry,
                "long",
            ),
            FrozenDirection::Short => (
                self.stop_loss > self.entry,
                self.take_profit < self.entry,
                "short",
            ),
        };
        if !stop_ok {
            return Err(format!(
                "a {side} position's stop_loss ({}) is on the wrong side of its entry ({}) \
                 — that is an instant exit, not a stop",
                self.stop_loss, self.entry
            ));
        }
        if !target_ok {
            return Err(format!(
                "a {side} position's take_profit ({}) is on the wrong side of its entry ({}) \
                 — the trade could only win by moving the wrong way",
                self.take_profit, self.entry
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operator's real ESPIX_EUR h1 TradeNation trade, as local-chart's
    /// `GET /arm-setup` emitted it on 2026-09-24.
    fn espix() -> FrozenPosition {
        FrozenPosition {
            direction: FrozenDirection::Long,
            entry: 19670.6,
            stop_loss: 19637.3,
            take_profit: 19910.3,
        }
    }

    #[test]
    fn parses_the_shape_local_chart_actually_emits() {
        let json =
            r#"{"direction":"long","entry":19670.6,"stop_loss":19637.3,"take_profit":19910.3}"#;
        let got: FrozenPosition = serde_json::from_str(json).expect("parse");
        assert_eq!(got, espix());
    }

    #[test]
    fn round_trips_exactly() {
        let text = serde_json::to_string(&espix()).expect("serialize");
        let back: FrozenPosition = serde_json::from_str(&text).expect("parse");
        assert_eq!(back, espix());
    }

    /// The key set is load-bearing on BOTH sides: local-chart must emit
    /// exactly these, and a stray key here means a producer bug went unseen.
    #[test]
    fn the_serialized_form_carries_every_field() {
        let v = serde_json::to_value(espix()).expect("serialize");
        let mut keys: Vec<&str> = v
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["direction", "entry", "stop_loss", "take_profit"]);
    }

    #[test]
    fn an_unknown_key_is_refused_not_ignored() {
        let json = r#"{"direction":"long","entry":1.0,"stop_loss":0.9,
                       "take_profit":1.2,"stopLevel":30}"#;
        let err = serde_json::from_str::<FrozenPosition>(json)
            .expect_err("a stray tick-offset key must not be silently dropped");
        assert!(err.to_string().contains("stopLevel"), "{err}");
    }

    /// The levels are used VERBATIM. If this ever starts multiplying by a
    /// tick size, the ESPIX entry becomes 196.706 and the trade is nonsense.
    #[test]
    fn levels_are_the_drawn_prices_with_no_tick_conversion() {
        let l = espix().levels();
        assert_eq!(l.entry, 19670.6);
        assert_eq!(l.stop_loss, 19637.3);
        assert_eq!(l.take_profit, 19910.3);
    }

    #[test]
    fn direction_maps_to_the_core_enum() {
        assert_eq!(espix().direction(), PositionDirection::Long);
        assert_eq!(
            FrozenPosition {
                direction: FrozenDirection::Short,
                ..espix()
            }
            .direction(),
            PositionDirection::Short
        );
    }

    #[test]
    fn the_operators_real_trade_validates() {
        espix().validate().expect("a real drawn trade is valid");
    }

    #[test]
    fn a_long_with_its_stop_above_entry_is_refused() {
        let bad = FrozenPosition {
            stop_loss: 19700.0,
            ..espix()
        };
        let err = bad.validate().expect_err("wrong-side stop");
        assert!(err.contains("stop_loss"), "{err}");
        assert!(err.contains("wrong side"), "{err}");
    }

    #[test]
    fn a_long_with_its_target_below_entry_is_refused() {
        let bad = FrozenPosition {
            take_profit: 19600.0,
            ..espix()
        };
        let err = bad.validate().expect_err("wrong-side target");
        assert!(err.contains("take_profit"), "{err}");
    }

    /// The mirror case: a SHORT whose levels are drawn long-ways round.
    /// local-chart exports direction from the drawing TYPE and never
    /// "corrects" it, precisely so this arrives here to be caught.
    #[test]
    fn a_short_drawn_long_ways_round_is_refused() {
        let bad = FrozenPosition {
            direction: FrozenDirection::Short,
            ..espix()
        };
        let err = bad.validate().expect_err("short with a long's levels");
        assert!(err.contains("short"), "{err}");
    }

    #[test]
    fn a_valid_short_passes() {
        FrozenPosition {
            direction: FrozenDirection::Short,
            entry: 1.1000,
            stop_loss: 1.1050,
            take_profit: 1.0900,
        }
        .validate()
        .expect("a well-formed short");
    }

    #[test]
    fn a_non_finite_price_is_refused() {
        let err = FrozenPosition {
            stop_loss: f64::NAN,
            ..espix()
        }
        .validate()
        .expect_err("NaN stop");
        assert!(err.contains("finite"), "{err}");
    }

    #[test]
    fn a_zero_or_negative_price_is_refused() {
        let err = FrozenPosition {
            entry: 0.0,
            ..espix()
        }
        .validate()
        .expect_err("zero entry");
        assert!(err.contains("positive"), "{err}");
    }

    #[test]
    fn direction_uses_the_lowercase_words_local_chart_emits() {
        let v = serde_json::to_value(espix()).expect("serialize");
        assert_eq!(v["direction"], serde_json::json!("long"));
    }
}
