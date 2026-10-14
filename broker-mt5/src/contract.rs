//! Monetary stop-loss risk for MT5 contracts, before lot rounding.
use trade_control_core::broker::EntryError;

/// MT5 wire values from ENUM_SYMBOL_CALC_MODE. These modes all use
/// (close - open) * contract size * lots in the symbol's profit currency.
/// Futures, options, bonds and collateral need other metadata/formulas.
pub fn validate_calc_mode(calc_mode: u32) -> Result<(), EntryError> {
    if matches!(calc_mode, 0 | 2..=5 | 32 | 38) {
        Ok(())
    } else {
        Err(EntryError::ContractSizeUnavailable)
    }
}

/// Convert one lot's price loss to account currency; never infer contract
/// size from the asset class or reuse the chart's point value.
pub fn loss_per_lot(
    calc_mode: u32,
    contract_size: f64,
    entry: f64,
    stop: f64,
    currency_rate: f64,
) -> Result<f64, EntryError> {
    validate_calc_mode(calc_mode)?;
    if ![contract_size, entry, stop, currency_rate]
        .into_iter()
        .all(|value| value.is_finite() && value > 0.0)
    {
        return Err(EntryError::OrderRejected);
    }
    let loss = (entry - stop).abs() * contract_size * currency_rate;
    if loss.is_finite() && loss > 0.0 {
        Ok(loss)
    } else {
        Err(EntryError::OrderRejected)
    }
}

#[cfg(test)]
mod tests;
