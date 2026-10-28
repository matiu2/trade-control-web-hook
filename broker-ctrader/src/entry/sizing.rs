use super::*;
mod conversion;
mod risk;
#[cfg(test)]
mod tests;

pub(super) struct SizedEntry {
    pub cents: i64,
    pub implied_pct: f64,
    pub max_stop_distance: f64,
}
pub(crate) fn volume(
    units: f64,
    min: i64,
    max: i64,
    step: i64,
) -> std::result::Result<i64, EntryError> {
    if !units.is_finite() || units <= 0.0 || min <= 0 || max < min || step <= 0 {
        return Err(reject("invalid cTrader volume or symbol limits"));
    }
    let raw = (units * 100.0).floor();
    if raw > i64::MAX as f64 {
        return Err(reject("cTrader volume overflow"));
    }
    let volume = (raw as i64 / step).min(max / step) * step;
    if volume < min {
        return Err(EntryError::UnitsBelowMinimum);
    }
    Ok(volume)
}
pub(super) async fn size(
    broker: &OpenApiBroker,
    req: &EntryRequest<'_>,
    symbol: &InstrumentMetadata,
    trader: &Value,
    price: f64,
    cap: f64,
) -> std::result::Result<SizedEntry, EntryError> {
    let rate = conversion::rate(
        broker,
        symbol.quote_asset,
        integer(trader, "depositAssetId").map_err(|_| EntryError::AccountFetch)?,
    )
    .await?;
    let equity = broker.equity().await.map_err(|_| EntryError::EquityParse)?;
    calculate(&broker.account_name, req, symbol, price, equity, rate, cap)
}

fn calculate(
    account: &str,
    req: &EntryRequest<'_>,
    symbol: &InstrumentMetadata,
    price: f64,
    equity: f64,
    rate: f64,
    cap: f64,
) -> std::result::Result<SizedEntry, EntryError> {
    if ![equity, rate, cap]
        .iter()
        .all(|v| v.is_finite() && *v > 0.0)
    {
        return Err(reject("invalid cTrader sizing snapshot"));
    }
    let (distance, _) = geometry::geometry(req, price)?;
    let effective_distance = if matches!(req.entry, ResolvedEntry::Market { .. }) {
        (distance * 100000.0 - 1e-7).ceil() / 100000.0
    } else {
        distance
    };
    let risk_per_unit = effective_distance * rate;
    let units = match req.risk {
        RiskBudget::Percent(p) => equity * p / 100.0 / risk_per_unit,
        RiskBudget::Amount(a) => a / risk_per_unit,
        RiskBudget::Units(u) => u,
    };
    let min = integer(&symbol.raw, "minVolume").map_err(|e| reject(e.to_string()))?;
    let max = integer(&symbol.raw, "maxVolume").map_err(|e| reject(e.to_string()))?;
    let step = integer(&symbol.raw, "stepVolume").map_err(|e| reject(e.to_string()))?;
    let cents = volume(units, min, max, step)?;
    let implied = (cents as f64 / 100.0) * risk_per_unit / equity * 100.0;
    let requested = match req.risk {
        RiskBudget::Percent(p) => p,
        RiskBudget::Amount(a) => a / equity * 100.0,
        RiskBudget::Units(_) => units * risk_per_unit / equity * 100.0,
    };
    if !requested.is_finite() || requested <= 0.0 {
        return Err(reject("risk budget must be finite and positive"));
    }
    if requested > cap + 1e-10 || implied > cap + 1e-10 {
        return Err(EntryError::RiskCapExceeded {
            requested: requested.max(implied),
            cap,
        });
    }
    let oversized = (units * 100.0).floor() as i64 / step > max / step;
    if oversized {
        let capped_units = cents as f64 / 100.0;
        tracing::warn!(
            account,
            instrument = %symbol.name,
            setup_id = req.setup_id,
            policy = ?req.on_oversize,
            requested_units = units,
            maximum_units = max as f64 / 100.0,
            capped_units,
            requested_risk_pct = requested,
            capped_risk_pct = implied,
            stop_loss = req.stop_loss,
            take_profit = req.take_profit,
            "cTrader requested volume exceeds symbol maximum; applying oversize policy; stop and target preserved"
        );
        if req.on_oversize == trade_control_core::intent::OversizePolicy::Reject {
            return Err(reject(format!(
                "cTrader volume exceeds symbol maximum; on_oversize=reject: requested {units} units, maximum {} units",
                max as f64 / 100.0
            )));
        }
    }
    Ok(SizedEntry {
        cents,
        implied_pct: implied,
        max_stop_distance: risk::max_stop_distance(&req.risk, equity, rate, cents, cap)?,
    })
}
