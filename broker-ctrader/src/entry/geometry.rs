use super::*;
pub(super) fn geometry(
    req: &EntryRequest<'_>,
    price: f64,
) -> std::result::Result<(f64, f64), EntryError> {
    if ![price, req.stop_loss, req.take_profit]
        .iter()
        .all(|v| v.is_finite() && *v > 0.0)
    {
        return Err(reject(
            "entry, stop and target must be finite positive prices",
        ));
    }
    let sign = if req.direction == Direction::Long {
        1.0
    } else {
        -1.0
    };
    let stop = (price - req.stop_loss) * sign;
    let target = (req.take_profit - price) * sign;
    if stop <= 0.0 || target <= 0.0 {
        return Err(reject(
            "stop/target are on the wrong side of executable entry price",
        ));
    }
    Ok((stop, target))
}
pub(crate) fn payload(
    account: i64,
    symbol: i64,
    volume: i64,
    req: &EntryRequest<'_>,
    price: f64,
) -> std::result::Result<Value, EntryError> {
    let (stop, target) = geometry(req, price)?;
    let mut p = json!({"ctidTraderAccountId":account,"symbolId":symbol,"volume":volume,"tradeSide":if req.direction==Direction::Long {1}else{2},"label":"trade-control"});
    match req.entry {
        ResolvedEntry::Market { .. } => {
            p["orderType"] = json!(1);
            p["timeInForce"] = json!(3);
            p["relativeStopLoss"] = json!((stop * 100000.0 - 1e-7).ceil() as i64);
            p["relativeTakeProfit"] = json!((target * 100000.0 + 1e-7).floor() as i64);
        }
        ResolvedEntry::Stop { trigger_price } => {
            p["orderType"] = json!(3);
            p["timeInForce"] = json!(2);
            p["stopPrice"] = json!(trigger_price);
            p["stopLoss"] = json!(req.stop_loss);
            p["takeProfit"] = json!(req.take_profit);
        }
        ResolvedEntry::Limit { trigger_price } => {
            p["orderType"] = json!(2);
            p["timeInForce"] = json!(2);
            p["limitPrice"] = json!(trigger_price);
            p["stopLoss"] = json!(req.stop_loss);
            p["takeProfit"] = json!(req.take_profit);
        }
    }
    if matches!(req.entry, ResolvedEntry::Market { .. })
        && (integer(&p, "relativeStopLoss").unwrap_or(0) <= 0
            || integer(&p, "relativeTakeProfit").unwrap_or(0) <= 0)
    {
        return Err(reject(
            "market protection distances below cTrader precision",
        ));
    }
    Ok(p)
}

pub(super) fn price(req: &EntryRequest<'_>, quote: Quote) -> std::result::Result<f64, EntryError> {
    let price = match req.entry {
        ResolvedEntry::Market { .. } => {
            if req.direction == Direction::Long {
                quote.ask
            } else {
                quote.bid
            }
        }
        _ => req.entry.reference_price(),
    };
    let executable = if req.direction == Direction::Long {
        quote.ask
    } else {
        quote.bid
    };
    let sign = if req.direction == Direction::Long {
        1.0
    } else {
        -1.0
    };
    match req.entry {
        ResolvedEntry::Stop { .. } if (price - executable) * sign <= 0.0 => {
            return Err(EntryError::EntryTooCloseToMarket);
        }
        ResolvedEntry::Limit { .. } if (executable - price) * sign <= 0.0 => {
            return Err(EntryError::EntryTooCloseToMarket);
        }
        _ => {}
    }
    Ok(price)
}
