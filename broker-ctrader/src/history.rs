use super::*;
use trade_control_core::{
    broker_exit::{BrokerTradeExit, ExitReason},
    settlement::{LedgerEntry, LedgerSource, SettledTrade},
};
use wire::{array, integer, is_enum, money, number, optional_number};

pub(crate) async fn position_orders(
    broker: &CtraderBroker,
    position: i64,
    now: DateTime<Utc>,
) -> Result<Vec<Value>> {
    position_history(
        broker,
        position,
        now,
        2183,
        2184,
        "order",
        "utcLastUpdateTimestamp",
    )
    .await
}
async fn position_deals(
    broker: &CtraderBroker,
    position: i64,
    now: DateTime<Utc>,
) -> Result<Vec<Value>> {
    position_history(
        broker,
        position,
        now,
        2179,
        2180,
        "deal",
        "executionTimestamp",
    )
    .await
}
async fn position_history(
    broker: &CtraderBroker,
    position: i64,
    now: DateTime<Utc>,
    request: u32,
    response: u32,
    key: &str,
    time_key: &str,
) -> Result<Vec<Value>> {
    let mut end = now.timestamp_millis();
    let mut result = Vec::new();
    for count in 0..1000 {
        let data = broker
            .read(
                request,
                response,
                json!({"positionId":position,"fromTimestamp":0,"toTimestamp":end}),
            )
            .await?;
        wire::array_shape(&data, key)?;
        let rows = array(&data, key);
        let oldest = rows
            .iter()
            .map(|row| integer(row, time_key))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .min();
        result.extend_from_slice(rows);
        let more = data["hasMore"]
            .as_bool()
            .ok_or_else(|| eyre!("cTrader position history missing hasMore"))?;
        if !more {
            result.sort_by_key(|row| {
                integer(row, if key == "deal" { "dealId" } else { "orderId" }).unwrap_or(0)
            });
            result.dedup_by(|a, b| a == b);
            return Ok(result);
        }
        let oldest =
            oldest.ok_or_else(|| eyre!("cTrader position history hasMore with no rows"))?;
        ensure!(
            oldest < end && count < 999,
            "cTrader position history pagination stalled; refusing incomplete history"
        );
        end = oldest;
    }
    Err(eyre!("cTrader position history pagination limit"))
}
pub(crate) fn origin(orders: &[Value], position: i64) -> Result<Option<&Value>> {
    let openings: Vec<_> = orders
        .iter()
        .filter(|order| {
            integer(order, "positionId").ok() == Some(position)
                && order["closingOrder"].as_bool() != Some(true)
                && is_enum(order, "orderStatus", 2, "ORDER_STATUS_FILLED")
        })
        .collect();
    ensure!(
        openings.len() <= 1,
        "cTrader position contains multiple entry orders; cannot attribute one plan safely"
    );
    Ok(openings.first().copied())
}
async fn details(
    broker: &CtraderBroker,
    instrument: &str,
    id: &str,
    position: Option<&str>,
) -> Result<(Value, i64)> {
    let id: i64 = id.parse()?;
    ensure!(id > 0, "invalid cTrader order ID");
    let symbol = metadata::instrument_metadata_id(broker, instrument).await?;
    let data = broker.read(2181, 2182, json!({"orderId":id})).await?;
    wire::array_shape(&data, "deal")?;
    let mut order = data["order"].clone();
    if order.get("executedVolume").is_none() {
        let filled = array(&data, "deal")
            .iter()
            .filter(|d| {
                is_enum(d, "dealStatus", 2, "FILLED")
                    || is_enum(d, "dealStatus", 3, "PARTIALLY_FILLED")
            })
            .map(|d| integer(d, "filledVolume"))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .sum::<i64>();
        order["executedVolume"] = json!(filled);
    }
    ensure!(
        integer(&order, "orderId")? == id && integer(&order["tradeData"], "symbolId")? == symbol,
        "cTrader history order does not match instrument/ID"
    );
    if let Some(position) = position {
        ensure!(
            integer(&order, "positionId")? == position.parse::<i64>()?,
            "cTrader order/position link mismatch"
        );
    }
    Ok((order, symbol))
}
pub(crate) async fn attempt(
    broker: &CtraderBroker,
    instrument: &str,
    id: &str,
    position: Option<&str>,
) -> Result<AttemptState> {
    let (order, symbol) = details(broker, instrument, id, position).await?;
    let snapshot = broker.snapshot().await?;
    let pid = integer(&order, "positionId").ok();
    if let Some(p) = array(&snapshot, "position").iter().find(|p| {
        integer(p, "positionId").ok() == pid
            && pid.is_some()
            && integer(&p["tradeData"], "symbolId").ok() == Some(symbol)
            && is_enum(p, "positionStatus", 1, "POSITION_STATUS_OPEN")
    }) {
        return Ok(AttemptState::OpenPosition {
            broker_trade_id: integer(p, "positionId")?.to_string(),
        });
    }
    let filled = integer(&order, "executedVolume")?;
    if cancelled_without_fill(&order, filled) {
        return Ok(AttemptState::Cancelled);
    }
    if array(&snapshot, "order")
        .iter()
        .any(|o| integer(o, "orderId").ok() == integer(&order, "orderId").ok())
    {
        return Ok(AttemptState::Pending);
    }
    if let Some(exit) = exit_from_order(broker, &order, symbol, Utc::now()).await? {
        return Ok(AttemptState::ClosedConfirmed {
            exit: Box::new(exit),
        });
    }
    Ok(AttemptState::Unknown)
}
fn validate_origin_deals(deals: &[Value], order: i64) -> Result<()> {
    ensure!(
        deals
            .iter()
            .filter(|d| d.get("closePositionDetail").is_none()
                && (is_enum(d, "dealStatus", 2, "FILLED")
                    || is_enum(d, "dealStatus", 3, "PARTIALLY_FILLED")))
            .all(|d| integer(d, "orderId").ok() == Some(order)),
        "cTrader position contains opening fills from another order; refusing combined P&L attribution"
    );
    Ok(())
}
pub(crate) fn cancelled_without_fill(order: &Value, filled: i64) -> bool {
    filled == 0
        && [
            (3, "ORDER_STATUS_REJECTED"),
            (4, "ORDER_STATUS_EXPIRED"),
            (5, "ORDER_STATUS_CANCELLED"),
        ]
        .iter()
        .any(|(n, name)| is_enum(order, "orderStatus", *n, name))
}
pub(crate) struct Closed {
    deals: Vec<Value>,
    volume: f64,
    pub(crate) net: Option<f64>,
    swap: f64,
    entry: f64,
    exit: f64,
    closed: DateTime<Utc>,
}
pub(crate) fn closed(deals: &[Value]) -> Result<Option<Closed>> {
    let filled: Vec<_> = deals
        .iter()
        .filter(|d| {
            is_enum(d, "dealStatus", 2, "FILLED") || is_enum(d, "dealStatus", 3, "PARTIALLY_FILLED")
        })
        .collect();
    let opening = filled
        .iter()
        .filter(|d| d.get("closePositionDetail").is_none())
        .map(|d| integer(d, "filledVolume"))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .sum::<i64>();
    let closing: Vec<_> = filled
        .iter()
        .filter(|d| d.get("closePositionDetail").is_some())
        .copied()
        .collect();
    let volume = closing
        .iter()
        .map(|d| integer(&d["closePositionDetail"], "closedVolume"))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .sum::<i64>();
    if opening <= 0 || volume != opening {
        return Ok(None);
    }
    let net = None;
    let mut swap = 0.0;
    let mut entry = 0.0;
    let mut exit = 0.0;
    let mut last = 0;
    for deal in &closing {
        let detail = &deal["closePositionDetail"];
        let quantity = number(detail, "closedVolume")?;
        let financing = money(detail, "swap")?;
        // Fee signs vary across platform brokers and are not verified against an
        // authenticated account yet. Preserve raw amounts; never invent net P&L.
        swap += financing;
        entry += number(detail, "entryPrice")? * quantity;
        exit += number(deal, "executionPrice")? * quantity;
        last = last.max(integer(deal, "executionTimestamp")?);
    }
    Ok(Some(Closed {
        deals: closing.into_iter().cloned().collect(),
        volume: volume as f64 / 100.0,
        net,
        swap,
        entry: entry / volume as f64,
        exit: exit / volume as f64,
        closed: DateTime::from_timestamp_millis(last)
            .ok_or_else(|| eyre!("invalid cTrader deal timestamp"))?,
    }))
}
async fn account_currency(broker: &CtraderBroker) -> Result<String> {
    let trader = metadata::trader(broker).await?;
    let asset = integer(&trader, "depositAssetId")?;
    metadata::assets(broker)
        .await?
        .iter()
        .find(|a| integer(a, "assetId").ok() == Some(asset))
        .and_then(|a| a["name"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| eyre!("cTrader account currency name unavailable"))
}
async fn exit_from_order(
    broker: &CtraderBroker,
    order: &Value,
    symbol: i64,
    now: DateTime<Utc>,
) -> Result<Option<BrokerTradeExit>> {
    let Some(position) = integer(order, "positionId").ok() else {
        return Ok(None);
    };
    let deals = position_deals(broker, position, now).await?;
    ensure!(
        deals
            .iter()
            .all(|d| integer(d, "positionId").ok() == Some(position)
                && integer(d, "symbolId").ok() == Some(symbol)),
        "cTrader deal history correlation mismatch"
    );
    validate_origin_deals(&deals, integer(order, "orderId")?)?;
    let Some(closed) = closed(&deals)? else {
        return Ok(None);
    };
    let currency = account_currency(broker).await?;
    let last = closed
        .deals
        .iter()
        .max_by_key(|d| integer(d, "executionTimestamp").unwrap_or(0));
    Ok(Some(BrokerTradeExit {
        broker_trade_id: position.to_string(),
        exit_order_id: last
            .and_then(|d| integer(d, "orderId").ok())
            .map(|i| i.to_string()),
        transaction_id: last
            .and_then(|d| integer(d, "dealId").ok())
            .map(|i| i.to_string()),
        closed_at: Some(closed.closed),
        reason: ExitReason::Unknown,
        broker_reason: Some(
            "cTrader complete matched closing-deal history; exit trigger not established".into(),
        ),
        entry_price: Some(closed.entry),
        exit_price: Some(closed.exit),
        expected_price: None,
        realized_pl: closed.net,
        account_currency: Some(currency),
        units: Some(closed.volume),
        quote_currency: None,
    }))
}
pub(crate) async fn trade_exit(
    broker: &CtraderBroker,
    instrument: &str,
    id: &str,
    position: Option<&str>,
) -> Result<Option<BrokerTradeExit>> {
    let (order, symbol) = details(broker, instrument, id, position).await?;
    exit_from_order(broker, &order, symbol, Utc::now()).await
}
pub(crate) async fn settlement(
    broker: &CtraderBroker,
    instrument: &str,
    _since: DateTime<Utc>,
    ids: &[String],
    now: DateTime<Utc>,
) -> Result<Settlement> {
    let mut result = Settlement {
        broker: "ctrader".into(),
        fetched_at: now,
        trades: Vec::new(),
        ledger: Vec::new(),
        warnings: Vec::new(),
    };
    if ids.is_empty() {
        result
            .warnings
            .push("No originating broker order IDs; cannot attribute cTrader settlement".into());
        return Ok(result);
    }
    let currency = account_currency(broker).await?;
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        let (order, symbol) = match details(broker, instrument, id, None).await {
            Ok(value) => value,
            Err(error) => {
                result
                    .warnings
                    .push(format!("Order {id} history unavailable: {error}"));
                continue;
            }
        };
        let Some(position) = integer(&order, "positionId").ok() else {
            continue;
        };
        if !seen.insert(position) {
            continue;
        }
        let deals = position_deals(broker, position, now).await?;
        ensure!(
            deals
                .iter()
                .all(|d| integer(d, "positionId").ok() == Some(position)
                    && integer(d, "symbolId").ok() == Some(symbol)),
            "settlement deal correlation mismatch"
        );
        validate_origin_deals(&deals, integer(&order, "orderId")?)?;
        for deal in &deals {
            result.ledger.push(LedgerEntry {
                source: LedgerSource::Activity,
                reference: integer(deal, "dealId").ok().map(|v| v.to_string()),
                occurred_at: integer(deal, "executionTimestamp")
                    .ok()
                    .and_then(DateTime::from_timestamp_millis),
                description: serde_json::to_string(deal)?,
                instrument: Some(instrument.into()),
                price: optional_number(deal, "executionPrice"),
                size: optional_number(deal, "filledVolume").map(|v| v / 100.0),
                amount: deal
                    .get("closePositionDetail")
                    .and_then(|d| money(d, "grossProfit").ok()),
                currency: Some(currency.clone()),
            });
        }
        if let Some(closed) = closed(&deals)? {
            result.warnings.push(format!("Position {position}: net P&L unavailable until commission and conversion-fee signs are verified; see raw deal ledger"));
            result.trades.push(SettledTrade {
                broker_trade_id: position.to_string(),
                broker_order_id: Some(id.clone()),
                instrument: Some(instrument.into()),
                entry_price: Some(closed.entry),
                exit_price: Some(closed.exit),
                size: Some(closed.volume),
                opened_at: integer(&order["tradeData"], "openTimestamp")
                    .ok()
                    .and_then(DateTime::from_timestamp_millis),
                closed_at: Some(closed.closed),
                realized_pl: closed.net,
                financing: Some(closed.swap),
                currency: Some(currency.clone()),
            });
        } else {
            result.warnings.push(format!(
                "Position {position}: complete closure not established; no final P&L reported"
            ));
        }
    }
    Ok(result)
}
