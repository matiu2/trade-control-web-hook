use super::*;
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
    let volume = (raw as i64 / step) * step;
    if volume < min {
        return Err(EntryError::UnitsBelowMinimum);
    }
    if volume > max {
        return Err(reject("risk-sized cTrader volume exceeds symbol maximum"));
    }
    Ok(volume)
}
async fn conversion(
    broker: &OpenApiBroker,
    from: i64,
    to: i64,
) -> std::result::Result<f64, EntryError> {
    if from == to {
        return Ok(1.0);
    }
    let symbols = metadata::symbols(broker)
        .await
        .map_err(|e| reject(format!("conversion symbol lookup: {e}")))?;
    let pairs: Vec<_> = symbols
        .iter()
        .filter(|s| {
            let b = integer(s, "baseAssetId").ok();
            let q = integer(s, "quoteAssetId").ok();
            (b == Some(from) && q == Some(to)) || (b == Some(to) && q == Some(from))
        })
        .collect();
    if pairs.len() != 1 {
        return Err(EntryError::CurrencyConversion {
            from: from.to_string(),
            to: to.to_string(),
            detail: "no unique direct conversion pair; cross conversion unsupported".into(),
        });
    }
    let pair = pairs[0];
    let name = pair["symbolName"]
        .as_str()
        .ok_or_else(|| reject("conversion symbol name missing"))?;
    let quote = metadata::quote(broker, name)
        .await
        .map_err(|e| reject(format!("conversion quote unavailable: {e}")))?;
    Ok(if integer(pair, "baseAssetId").ok() == Some(from) {
        quote.ask
    } else {
        1.0 / quote.bid
    })
}
pub(super) async fn size(
    broker: &OpenApiBroker,
    req: &EntryRequest<'_>,
    symbol: &InstrumentMetadata,
    trader: &Value,
    price: f64,
    cap: f64,
) -> std::result::Result<(i64, f64), EntryError> {
    let (distance, _) = geometry::geometry(req, price)?;
    let effective_distance = if matches!(req.entry, ResolvedEntry::Market { .. }) {
        (distance * 100000.0 - 1e-7).ceil() / 100000.0
    } else {
        distance
    };
    let rate = conversion(
        broker,
        symbol.quote_asset,
        integer(trader, "depositAssetId").map_err(|_| EntryError::AccountFetch)?,
    )
    .await?;
    let equity = broker.equity().await.map_err(|_| EntryError::EquityParse)?;
    let risk_per_unit = effective_distance * rate;
    let units = match req.risk {
        RiskBudget::Percent(p) => equity * p / 100.0 / risk_per_unit,
        RiskBudget::Amount(a) => a / risk_per_unit,
        RiskBudget::Units(u) => u,
    };
    let cents = volume(
        units,
        integer(&symbol.raw, "minVolume").map_err(|e| reject(e.to_string()))?,
        integer(&symbol.raw, "maxVolume").map_err(|e| reject(e.to_string()))?,
        integer(&symbol.raw, "stepVolume").map_err(|e| reject(e.to_string()))?,
    )?;
    let implied = (cents as f64 / 100.0) * risk_per_unit / equity * 100.0;
    let requested = match req.risk {
        RiskBudget::Percent(p) => p,
        RiskBudget::Amount(a) => a / equity * 100.0,
        RiskBudget::Units(_) => implied,
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
    Ok((cents, implied))
}
