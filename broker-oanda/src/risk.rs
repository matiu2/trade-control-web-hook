//! Pure position-sizing and risk-aggregation math.
//!
//! Cross-currency sizing is handled by passing an explicit FX rate
//! (quote → account) into [`units_for_budget`]. Same-currency pairs
//! pass `1.0` and behave exactly as before. See [`crate::fx`] for
//! resolving the rate against OANDA's pricing endpoint.

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
///
/// `fx_quote_to_account` converts one unit of the instrument's quote
/// currency into the account currency. Pass `1.0` when they match.
///
/// Retained as a thin wrapper around [`units_for_budget`] so existing
/// tests keep working; the worker now resolves percent vs amount at
/// the call site and calls `units_for_budget` directly.
#[allow(dead_code)]
pub fn units_for_risk(
    equity: f64,
    risk_pct: f64,
    entry: f64,
    stop_loss: f64,
    fx_quote_to_account: f64,
) -> u32 {
    let stop_distance = (entry - stop_loss).abs();
    if stop_distance <= 0.0 || equity <= 0.0 || risk_pct <= 0.0 {
        return 0;
    }
    let budget = equity * risk_pct / 100.0;
    units_for_budget(budget, entry, stop_loss, fx_quote_to_account)
}

/// Position size in units for a budget already in account currency.
/// Returned units are floored. Used by both the percent-of-equity path
/// (after `equity * pct / 100`) and the fixed-amount path.
///
/// `fx_quote_to_account` is the rate from the instrument's quote
/// currency into the account currency (e.g. for an AUD account on
/// NZD_CHF, this is the AUD value of one CHF — typically `1 /
/// mid(AUD_CHF)`). Pass `1.0` when account ccy == quote ccy.
///
/// Math: stop loss in account currency is
/// `stop_distance * units * fx_quote_to_account`. Setting that equal
/// to `budget` and solving for units gives `budget / (stop_distance *
/// rate)`.
pub fn units_for_budget(budget: f64, entry: f64, stop_loss: f64, fx_quote_to_account: f64) -> u32 {
    let stop_distance = (entry - stop_loss).abs();
    if stop_distance <= 0.0 || budget <= 0.0 || !budget.is_finite() {
        return 0;
    }
    if fx_quote_to_account <= 0.0 || !fx_quote_to_account.is_finite() {
        return 0;
    }
    let units = budget / (stop_distance * fx_quote_to_account);
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
        // budget = $100, stop distance = 0.1, units = 1000. Same-ccy rate = 1.0.
        assert_eq!(units_for_risk(10_000.0, 1.0, 100.0, 99.9, 1.0), 1_000);
    }

    #[test]
    fn units_for_risk_rounds_down() {
        // budget = $100, stop distance = 0.003 → 33333.33 → 33333
        assert_eq!(units_for_risk(10_000.0, 1.0, 1.1000, 1.0970, 1.0), 33_333);
    }

    #[test]
    fn units_for_risk_zero_stop_distance_yields_zero() {
        assert_eq!(units_for_risk(10_000.0, 1.0, 1.1000, 1.1000, 1.0), 0);
    }

    #[test]
    fn units_for_risk_zero_equity_yields_zero() {
        assert_eq!(units_for_risk(0.0, 1.0, 1.1, 1.09, 1.0), 0);
    }

    #[test]
    fn units_for_risk_zero_risk_yields_zero() {
        assert_eq!(units_for_risk(10_000.0, 0.0, 1.1, 1.09, 1.0), 0);
    }

    #[test]
    fn units_for_budget_basic() {
        // $100 budget, 0.1 stop distance → 1000 units.
        assert_eq!(units_for_budget(100.0, 100.0, 99.9, 1.0), 1_000);
    }

    #[test]
    fn units_for_budget_rounds_down() {
        // $1 budget, 0.003 stop distance → 333.33 → 333 units. Bet $1
        // on a 30-pip stop — useful smoke test for the fixed-amount mode.
        assert_eq!(units_for_budget(1.0, 1.1000, 1.0970, 1.0), 333);
    }

    #[test]
    fn units_for_budget_zero_budget_yields_zero() {
        assert_eq!(units_for_budget(0.0, 1.1, 1.09, 1.0), 0);
    }

    #[test]
    fn units_for_budget_negative_budget_yields_zero() {
        assert_eq!(units_for_budget(-1.0, 1.1, 1.09, 1.0), 0);
    }

    #[test]
    fn units_for_budget_nan_budget_yields_zero() {
        assert_eq!(units_for_budget(f64::NAN, 1.1, 1.09, 1.0), 0);
    }

    #[test]
    fn units_for_budget_zero_stop_distance_yields_zero() {
        assert_eq!(units_for_budget(100.0, 1.1, 1.1, 1.0), 0);
    }

    #[test]
    fn units_for_budget_zero_fx_yields_zero() {
        // Zero/negative/non-finite FX rates should refuse to size, not
        // produce u32::MAX or some other surprise.
        assert_eq!(units_for_budget(100.0, 1.1, 1.09, 0.0), 0);
        assert_eq!(units_for_budget(100.0, 1.1, 1.09, -1.0), 0);
        assert_eq!(units_for_budget(100.0, 1.1, 1.09, f64::NAN), 0);
        assert_eq!(units_for_budget(100.0, 1.1, 1.09, f64::INFINITY), 0);
    }

    #[test]
    fn units_for_budget_cross_currency_aud_account_nzd_chf() {
        // The real-world scenario the bug was discovered on:
        //
        // - account ccy = AUD, equity around 391,208.7166 AUD
        // - risk_amount = 10 AUD (fixed amount)
        // - instrument = NZD_CHF, quote ccy = CHF
        // - entry_ref = 0.46132, sl = 0.46194 → stop_distance = 0.00062 (CHF/NZD)
        // - AUD_CHF mid ≈ 0.5597 → 1 CHF ≈ 1.787 AUD (fx_quote_to_account)
        //
        // With FX, units = 10 / (0.00062 * 1.787) ≈ 9024.97 → floor →
        // 9025 (the actual product comes out a hair above 9024.97 in
        // f64). Before this fix the same call returned 16129, which
        // translates to ~17.87 AUD actual risk — 79% oversized.
        let units = units_for_budget(10.0, 0.46132, 0.46194, 1.787);
        assert_eq!(units, 9025);
    }

    #[test]
    fn units_for_budget_cross_currency_back_compat_same_ccy() {
        // Passing rate=1.0 must reproduce the legacy same-currency
        // behaviour bit-for-bit so we know nothing existing has shifted.
        // $100 budget, 0.1 stop → 1000 units, same as the basic case.
        assert_eq!(units_for_budget(100.0, 100.0, 99.9, 1.0), 1_000);
    }

    #[test]
    fn units_for_budget_cross_currency_jpy_account_eur_usd() {
        // JPY account trading EUR_USD. Quote ccy = USD.
        // EUR_JPY ≈ 165 → USD/JPY ≈ 150 → 1 USD = 150 JPY.
        // Risk 15,000 JPY, 50-pip USD stop (0.0050) → 15000 / (0.0050 * 150)
        // = 20000 units of EUR. f64 lands at 19999.999... → floor → 19999.
        assert_eq!(units_for_budget(15_000.0, 1.10, 1.0950, 150.0), 19_999);
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
