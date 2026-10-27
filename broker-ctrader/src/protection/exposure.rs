use super::*;
use adaptive_risk::{Exposure, LossEstimate, Side};
use std::collections::BTreeMap;

pub(super) async fn quote(
    b: &OpenApiBroker,
    symbol: i64,
    p: &PropAccount,
    oldest: &mut DateTime<Utc>,
) -> Result<Quote> {
    let (q, at) = b
        .shared
        .risk_quote(symbol, p.risk.execution.max_data_age_seconds)
        .await?;
    *oldest = (*oldest).min(at);
    Ok(q)
}
pub(super) async fn conversion(
    b: &OpenApiBroker,
    from: i64,
    to: i64,
    p: &PropAccount,
    oldest: &mut DateTime<Utc>,
    profit: bool,
) -> Result<Decimal> {
    if from == to {
        return Ok(Decimal::ONE);
    }
    let symbols = metadata::symbols(b).await?;
    let pairs: Vec<_> = symbols
        .iter()
        .filter(|s| {
            let base = wire::integer(s, "baseAssetId").ok();
            let quote = wire::integer(s, "quoteAssetId").ok();
            (base == Some(from) && quote == Some(to)) || (base == Some(to) && quote == Some(from))
        })
        .collect();
    ensure!(
        pairs.len() == 1,
        "unique direct account conversion unavailable"
    );
    let pair = pairs[0];
    let q = quote(b, wire::integer(pair, "symbolId")?, p, oldest).await?;
    if wire::integer(pair, "baseAssetId")? == from {
        decimal(if profit { q.bid } else { q.ask })
    } else {
        divide(Decimal::ONE, decimal(if profit { q.ask } else { q.bid })?)
    }
}
pub(super) fn estimate(
    side: Side,
    price: Decimal,
    stop: Decimal,
    conversion: Decimal,
    cost: &Costs,
    open: bool,
) -> Result<LossEstimate> {
    let opening = if open {
        Decimal::ZERO
    } else {
        cost.opening_per_unit
    };
    Ok(LossEstimate {
        side,
        price,
        stop: Some(stop),
        contract_size: Decimal::ONE,
        conversion,
        costs_per_unit: plus(
            plus(opening, cost.closing_per_unit)?,
            cost.future_swap_per_unit,
        )?,
        fixed_costs: cost.fixed_per_order,
        is_open_position: open,
    })
}
pub(super) async fn existing(
    b: &OpenApiBroker,
    p: &PropAccount,
    snapshot: &Value,
    trader: &Value,
    links: &BTreeMap<i64, String>,
    oldest: &mut DateTime<Utc>,
    unrealized: &Value,
) -> Result<Vec<Exposure>> {
    let symbols = metadata::symbols(b).await?;
    let assets = metadata::assets(b).await?;
    let currency = |id: i64| {
        assets
            .iter()
            .find(|a| wire::integer(a, "assetId").ok() == Some(id))
            .and_then(|a| a["name"].as_str())
            .is_some_and(metadata::currency)
    };
    let mut result = Vec::new();
    for (key, open) in [("position", true), ("order", false)] {
        for item in wire::array(snapshot, key) {
            let data = &item["tradeData"];
            let symbol = wire::integer(data, "symbolId")?;
            let light = symbols
                .iter()
                .find(|s| wire::integer(s, "symbolId").ok() == Some(symbol))
                .ok_or_else(|| eyre!("exposure instrument missing"))?;
            ensure!(
                currency(wire::integer(light, "baseAssetId")?)
                    && currency(wire::integer(light, "quoteAssetId")?),
                "existing non-FX exposure needs verified contract/tick valuation"
            );
            let name = light["symbolName"]
                .as_str()
                .ok_or_else(|| eyre!("exposure instrument name missing"))?;
            let cost = p
                .costs
                .get(name)
                .ok_or_else(|| eyre!("no verified future charge estimate for {name}"))?;
            let side = if wire::is_enum(data, "tradeSide", 1, "BUY") {
                Side::Buy
            } else if wire::is_enum(data, "tradeSide", 2, "SELL") {
                Side::Sell
            } else {
                return Err(eyre!("invalid exposure side"));
            };
            let q = quote(b, symbol, p, oldest).await?;
            let mut price = if open {
                decimal(if side == Side::Buy { q.bid } else { q.ask })?
            } else if item.get("limitPrice").is_some() {
                number(item, "limitPrice")?
            } else {
                number(item, "stopPrice")?
            };
            let stop = number(item, "stopLoss")?;
            if open {
                ensure!(
                    if side == Side::Buy {
                        price > stop
                    } else {
                        price < stop
                    },
                    "protective stop already crossed in fresh broker quote"
                );
            }
            let entry = if open { number(item, "price")? } else { price };
            let profit = open
                && if side == Side::Buy {
                    stop > entry
                } else {
                    stop < entry
                };
            let rate = conversion(
                b,
                wire::integer(light, "quoteAssetId")?,
                wire::integer(trader, "depositAssetId")?,
                p,
                oldest,
                profit,
            )
            .await?;
            let request = if open {
                links.get(&wire::integer(item, "positionId")?).cloned()
            } else {
                item["clientOrderId"].as_str().map(str::to_owned)
            };
            // Pending tradeData.volume is original requested size; subtract already executed
            // quantity so filled positions and their remaining order are not counted twice.
            let filled = if open {
                Decimal::ZERO
            } else {
                number(item, "executedVolume")?
            };
            let volume = divide(minus(number(data, "volume")?, filled)?, Decimal::from(100))?;
            ensure!(volume >= Decimal::ZERO, "pending volume regressed");
            if volume.is_zero() {
                continue;
            }
            if open {
                let pos = wire::integer(item, "positionId")?;
                let row = wire::array(unrealized, "positionUnrealizedPnL")
                    .iter()
                    .find(|row| wire::integer(row, "positionId").ok() == Some(pos))
                    .ok_or_else(|| eyre!("position gross PnL missing"))?;
                let digits = wire::integer(unrealized, "moneyDigits")?;
                ensure!((0..=12).contains(&digits), "invalid PnL precision");
                let gross = divide(
                    number(row, "grossUnrealizedPnL")?,
                    Decimal::from(10u64.pow(digits as u32)),
                )?;
                price = mark_from_pnl(entry, gross, volume, rate, side)?;
            }
            result.push(Exposure {
                broker_id: format!(
                    "{key}:{}",
                    wire::integer(item, if open { "positionId" } else { "orderId" })?
                ),
                request_id: request,
                volume,
                estimate: estimate(side, price, number(item, "stopLoss")?, rate, cost, open)?,
                overlapping: cost.overlapping,
            });
        }
    }
    Ok(result)
}

// A monetary mark derived from the SAME PnL message used for equity prevents
// asynchronous quote/PnL samples from inventing spendable risk headroom.
pub(super) fn mark_from_pnl(
    entry: Decimal,
    gross: Decimal,
    volume: Decimal,
    rate: Decimal,
    side: Side,
) -> Result<Decimal> {
    let movement = divide(gross, times(volume, rate)?)?;
    if side == Side::Buy {
        plus(entry, movement)
    } else {
        minus(entry, movement)
    }
}
