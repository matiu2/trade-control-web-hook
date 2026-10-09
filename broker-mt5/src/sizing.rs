use mt5_data_source::web::trading::{Lots, Rules};
use trade_control_core::{broker::EntryError, intent::RiskBudget};

/// Literal units on this broker are LOTS. Percent/amount size monetary SL risk.
pub(crate) fn lots(risk: RiskBudget, equity: f64, loss_per_lot: f64, cap: f64, rules: &Rules) -> Result<Lots, EntryError> {
    if !equity.is_finite() || equity <= 0.0 { return Err(EntryError::EquityParse); }
    if !loss_per_lot.is_finite() || loss_per_lot <= 0.0 || !cap.is_finite() || cap <= 0.0 { return Err(EntryError::OrderRejected); }
    let budget = match risk { RiskBudget::Percent(p) => equity * p / 100.0, RiskBudget::Amount(a) => a, RiskBudget::Units(l) => l * loss_per_lot };
    if !budget.is_finite() || budget <= 0.0 { return Err(EntryError::OrderRejected); }
    let pct = budget / equity * 100.0;
    if pct > cap + 1e-10 { return Err(EntryError::RiskCapExceeded { requested:pct, cap }); }
    if rules.volume_step_raw == 0 || rules.min_volume_raw == 0 || rules.max_volume_raw < rules.min_volume_raw { return Err(EntryError::OrderRejected); }
    let quantity = match risk { RiskBudget::Units(l) => l, _ => budget / loss_per_lot };
    let raw = (quantity * Lots::SCALE as f64).floor().min(rules.max_volume_raw as f64);
    if raw < rules.min_volume_raw as f64 || raw >= u64::MAX as f64 { return Err(EntryError::UnitsBelowMinimum); }
    let raw = raw as u64 / rules.volume_step_raw * rules.volume_step_raw;
    if raw < rules.min_volume_raw { return Err(EntryError::UnitsBelowMinimum); }
    Lots::from_raw(raw).map_err(|_| EntryError::UnitsBelowMinimum)
}
