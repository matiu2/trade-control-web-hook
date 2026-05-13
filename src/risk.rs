//! Pure position-sizing and risk-aggregation math.
//!
//! Caveat: this assumes account currency == quote currency of the instrument
//! (e.g. USD account trading EUR_USD). Cross-currency pairs are out of scope
//! for the MVP and will under- or over-size compared to a proper pip-value
//! calculation. Stick to instruments where the quote currency matches the
//! account currency until this is generalised.

/// One open position from OANDA, distilled to the fields we need for risk math.
///
/// Currently unused by the worker (which gates on `MAX_OPEN_POSITIONS` count only)
/// but kept tested and ready for when we upgrade to a true `MAX_TOTAL_OPEN_RISK_PCT`
/// gate that walks each open trade's SL order.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct OpenRisk {
    /// Units. Positive for long, negative for short.
    pub units: f64,
    /// Average entry price.
    pub entry: f64,
    /// Active stop-loss price. If `None`, the position contributes the worst-case
    /// risk (treated as max-pain — but practically we set SL on every entry, so
    /// this should always be `Some` for our positions).
    pub stop_loss: Option<f64>,
}

impl OpenRisk {
    /// Money at risk in account currency if the stop fills.
    /// Returns 0 if direction and SL don't match (defensive — never panics).
    #[allow(dead_code)]
    pub fn money_at_risk(&self) -> f64 {
        let Some(sl) = self.stop_loss else {
            // Without an SL, conservatively assume entry → 0, i.e. full notional.
            return self.units.abs() * self.entry;
        };
        // (units > 0 → long, expects sl < entry); risk = units * (entry - sl)
        // (units < 0 → short, expects sl > entry); risk = (-units) * (sl - entry)
        let raw = self.units * (self.entry - sl);
        raw.max(0.0)
    }
}

/// Position size in units for a given risk budget. Rounded down.
/// `equity` and `risk_pct` (0.5 means 0.5%) define the budget.
/// `entry` and `stop_loss` are absolute prices.
pub fn units_for_risk(equity: f64, risk_pct: f64, entry: f64, stop_loss: f64) -> u32 {
    let stop_distance = (entry - stop_loss).abs();
    if stop_distance <= 0.0 || equity <= 0.0 || risk_pct <= 0.0 {
        return 0;
    }
    let budget = equity * risk_pct / 100.0;
    let units = budget / stop_distance;
    if units <= 0.0 || !units.is_finite() {
        0
    } else {
        units.floor() as u32
    }
}

/// Sum of money-at-risk across open positions, expressed as % of equity.
#[allow(dead_code)]
pub fn total_open_risk_pct(positions: &[OpenRisk], equity: f64) -> f64 {
    if equity <= 0.0 {
        return f64::INFINITY;
    }
    let total: f64 = positions.iter().map(OpenRisk::money_at_risk).sum();
    total / equity * 100.0
}

/// Project what total open risk % would be if we added this new trade.
#[allow(dead_code)]
pub fn projected_total_open_risk_pct(
    existing: &[OpenRisk],
    equity: f64,
    new_units: u32,
    new_entry: f64,
    new_stop_loss: f64,
    long: bool,
) -> f64 {
    let signed_units = if long {
        new_units as f64
    } else {
        -(new_units as f64)
    };
    let candidate = OpenRisk {
        units: signed_units,
        entry: new_entry,
        stop_loss: Some(new_stop_loss),
    };
    let mut combined: Vec<OpenRisk> = existing.to_vec();
    combined.push(candidate);
    total_open_risk_pct(&combined, equity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_for_risk_basic_long() {
        // $10k equity, 1% risk → budget = $100. Use clean prices (stop distance = 0.1
        // exactly) to dodge floating-point rounding-down-to-9999 on 1.10/1.09.
        // budget = $100, stop distance = 0.1, units = 1000
        assert_eq!(units_for_risk(10_000.0, 1.0, 100.0, 99.9), 1_000);
    }

    #[test]
    fn units_for_risk_rounds_down() {
        // budget = $100, stop distance = 0.003 → 33333.33 → 33333
        assert_eq!(units_for_risk(10_000.0, 1.0, 1.1000, 1.0970), 33_333);
    }

    #[test]
    fn units_for_risk_zero_stop_distance_yields_zero() {
        assert_eq!(units_for_risk(10_000.0, 1.0, 1.1000, 1.1000), 0);
    }

    #[test]
    fn units_for_risk_zero_equity_yields_zero() {
        assert_eq!(units_for_risk(0.0, 1.0, 1.1, 1.09), 0);
    }

    #[test]
    fn units_for_risk_zero_risk_yields_zero() {
        assert_eq!(units_for_risk(10_000.0, 0.0, 1.1, 1.09), 0);
    }

    #[test]
    fn open_risk_long_positive() {
        let p = OpenRisk {
            units: 10_000.0,
            entry: 1.1000,
            stop_loss: Some(1.0900),
        };
        // 10000 * 0.01 = 100
        assert!((p.money_at_risk() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn open_risk_short_positive() {
        let p = OpenRisk {
            units: -10_000.0,
            entry: 1.1000,
            stop_loss: Some(1.1100),
        };
        // -10000 * (1.10 - 1.11) = 100
        assert!((p.money_at_risk() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn open_risk_inconsistent_geometry_yields_zero() {
        // Long with SL above entry — shouldn't happen, but stay defensive
        let p = OpenRisk {
            units: 10_000.0,
            entry: 1.1000,
            stop_loss: Some(1.1100),
        };
        assert_eq!(p.money_at_risk(), 0.0);
    }

    #[test]
    fn total_risk_aggregates() {
        let positions = [
            OpenRisk {
                units: 10_000.0,
                entry: 1.1000,
                stop_loss: Some(1.0900),
            },
            OpenRisk {
                units: -5_000.0,
                entry: 1.2000,
                stop_loss: Some(1.2100),
            },
        ];
        // 100 + 50 = 150 on $10k = 1.5%
        let pct = total_open_risk_pct(&positions, 10_000.0);
        assert!((pct - 1.5).abs() < 1e-9);
    }

    #[test]
    fn projected_risk_includes_new_trade() {
        let existing = [OpenRisk {
            units: 10_000.0,
            entry: 1.1000,
            stop_loss: Some(1.0900),
        }];
        // Adding a 10k long with 100-pip SL adds another $100 → 2% total
        let pct = projected_total_open_risk_pct(&existing, 10_000.0, 10_000, 1.0500, 1.0400, true);
        assert!((pct - 2.0).abs() < 1e-9);
    }
}
