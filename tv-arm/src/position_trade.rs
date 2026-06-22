//! Convert a drawn TradingView position tool into absolute entry / SL /
//! TP prices.
//!
//! A `short_position` / `long_position` drawing stores:
//! - `points[0].price` — the **entry** price (absolute), and
//! - `properties.stop_level` / `properties.profit_level` — the SL/TP
//!   distances **in instrument ticks**, *not* absolute prices.
//!
//! The absolute stop/target are therefore `entry ± level × tick_size`,
//! with the sign set by direction:
//!
//! | direction | stop      | target    |
//! |-----------|-----------|-----------|
//! | short     | above ↑   | below ↓   |
//! | long      | below ↓   | above ↑   |
//!
//! `tick_size` comes from `instrument-lookup` (`Asset::tick_size`) — it
//! is **not** the pip size (fractional-pip FX quotes 10× finer than a
//! pip; for indices/gold tick == pip). The conversion is the single
//! place that knowledge is applied, so the rest of the pipeline deals in
//! absolute prices only.

use color_eyre::eyre::{Result, eyre};

use crate::roles::{PositionDirection, PositionDrawing};

/// Absolute prices derived from a position-tool drawing, ready to drop
/// onto an enter intent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionLevels {
    /// Entry price the operator anchored the tool at.
    pub entry: f64,
    /// Absolute stop-loss price.
    pub stop_loss: f64,
    /// Absolute take-profit price.
    pub take_profit: f64,
}

/// Resolve the drawing's entry anchor + tick distances into absolute
/// SL/TP prices using `tick_size`.
///
/// Errors when the drawing is missing the entry anchor or either tick
/// level (the classifier already filters those out, but resolving
/// defensively keeps the function total), or when `tick_size` is not a
/// positive finite number.
pub fn resolve_levels(pos: &PositionDrawing, tick_size: f64) -> Result<PositionLevels> {
    if !(tick_size.is_finite() && tick_size > 0.0) {
        return Err(eyre!(
            "tick_size must be positive and finite, got {tick_size}"
        ));
    }
    let entry = pos
        .drawing
        .points
        .first()
        .map(|p| p.price)
        .ok_or_else(|| eyre!("position tool has no entry anchor"))?;
    let stop_level = pos
        .drawing
        .properties
        .stop_level
        .ok_or_else(|| eyre!("position tool has no stopLevel"))?;
    let profit_level = pos
        .drawing
        .properties
        .profit_level
        .ok_or_else(|| eyre!("position tool has no profitLevel"))?;

    let stop_dist = stop_level * tick_size;
    let profit_dist = profit_level * tick_size;

    let (stop_loss, take_profit) = match pos.direction {
        // Short: stop above entry, target below.
        PositionDirection::Short => (entry + stop_dist, entry - profit_dist),
        // Long: stop below entry, target above.
        PositionDirection::Long => (entry - stop_dist, entry + profit_dist),
    };

    Ok(PositionLevels {
        entry,
        stop_loss,
        take_profit,
    })
}

/// Map the drawing direction onto the core/cli [`Direction`] used by the
/// enter intent.
///
/// [`Direction`]: trade_control_core::intent::Direction
pub fn core_direction(dir: PositionDirection) -> trade_control_core::intent::Direction {
    match dir {
        PositionDirection::Long => trade_control_core::intent::Direction::Long,
        PositionDirection::Short => trade_control_core::intent::Direction::Short,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trading_view::drawings::{Drawing, Point, Properties};

    fn pos(
        direction: PositionDirection,
        entry: f64,
        stop_level: f64,
        profit_level: f64,
    ) -> PositionDrawing {
        PositionDrawing {
            direction,
            drawing: Drawing {
                id: "p".into(),
                points: vec![Point {
                    time: 1,
                    price: entry,
                }],
                properties: Properties {
                    text: None,
                    stop_level: Some(stop_level),
                    profit_level: Some(profit_level),
                    qty: Some(0.01),
                },
            },
        }
    }

    #[test]
    fn short_de40_levels() {
        // Real DE40 short tool (R5zDSP): entry 23475, stopLevel 3000,
        // profitLevel 7007, tick 0.1. SL above, TP below.
        let levels = resolve_levels(&pos(PositionDirection::Short, 23475.0, 3000.0, 7007.0), 0.1)
            .expect("resolve");
        assert_eq!(levels.entry, 23475.0);
        assert!((levels.stop_loss - 23775.0).abs() < 1e-6, "{levels:?}");
        assert!((levels.take_profit - 22774.3).abs() < 1e-6, "{levels:?}");
        // Sanity: short ⇒ stop above entry above target.
        assert!(levels.stop_loss > levels.entry);
        assert!(levels.take_profit < levels.entry);
    }

    #[test]
    fn long_de40_levels() {
        // Real DE40 long tool (9BbQdH): entry 24195.3, stopLevel 801,
        // profitLevel 2223, tick 0.1. SL below, TP above.
        let levels = resolve_levels(&pos(PositionDirection::Long, 24195.3, 801.0, 2223.0), 0.1)
            .expect("resolve");
        assert!((levels.stop_loss - 24115.2).abs() < 1e-6, "{levels:?}");
        assert!((levels.take_profit - 24417.6).abs() < 1e-6, "{levels:?}");
        assert!(levels.stop_loss < levels.entry);
        assert!(levels.take_profit > levels.entry);
    }

    #[test]
    fn fractional_pip_fx_uses_tick_not_pip() {
        // EURUSD tick = 0.00001 (pip would be 0.0001). A 200-tick stop
        // is 20 pips = 0.0020, not 0.02. Guards against the pip/tick mixup.
        let levels = resolve_levels(
            &pos(PositionDirection::Short, 1.1000, 200.0, 400.0),
            0.00001,
        )
        .expect("resolve");
        assert!((levels.stop_loss - 1.1020).abs() < 1e-9, "{levels:?}");
        assert!((levels.take_profit - 1.0960).abs() < 1e-9, "{levels:?}");
    }

    #[test]
    fn rejects_non_positive_tick() {
        let err =
            resolve_levels(&pos(PositionDirection::Long, 100.0, 10.0, 20.0), 0.0).unwrap_err();
        assert!(format!("{err}").contains("tick_size"));
    }

    #[test]
    fn core_direction_maps() {
        use trade_control_core::intent::Direction;
        assert_eq!(core_direction(PositionDirection::Long), Direction::Long);
        assert_eq!(core_direction(PositionDirection::Short), Direction::Short);
    }
}
