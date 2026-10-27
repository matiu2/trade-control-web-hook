use super::*;
use adaptive_risk::ledger::Execution;
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Ledger {
    pub events: Vec<Execution>,
    pub position_requests: BTreeMap<i64, String>,
    pub orders: Vec<Value>,
    pub request_setups: BTreeMap<String, String>,
    pub account_breached: bool,
    pub daily_breached: BTreeSet<DateTime<Utc>>,
    pub high: Decimal,
    pub boundary_balance: Decimal,
}
async fn pages(
    b: &OpenApiBroker,
    kind: u32,
    response: u32,
    key: &str,
    time_key: &str,
    from: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<Value>> {
    let mut end = until.timestamp_millis();
    let mut result = BTreeMap::new();
    for _ in 0..1000 {
        let data = b
            .read(
                kind,
                response,
                json!({"fromTimestamp":from.timestamp_millis(),"toTimestamp":end}),
            )
            .await?;
        wire::array_shape(&data, key)?;
        let rows = wire::array(&data, key);
        let mut oldest = end;
        for row in rows {
            let time = wire::integer(row, time_key)?;
            ensure!(
                time >= from.timestamp_millis() && time <= end,
                "history outside requested interval"
            );
            oldest = oldest.min(time);
            let id = wire::integer(row, if key == "deal" { "dealId" } else { "orderId" })?;
            if let Some(previous) = result.insert(id, row.clone()) {
                ensure!(previous == *row, "conflicting broker history duplicate");
            }
        }
        let more = data["hasMore"]
            .as_bool()
            .ok_or_else(|| eyre!("history completeness missing"))?;
        if !more {
            return Ok(result.into_values().collect());
        }
        ensure!(
            !rows.is_empty() && oldest < end,
            "history pagination stalled; refusing incomplete history"
        );
        end = oldest;
    }
    Err(eyre!("history pagination limit reached"))
}
pub(super) async fn fetch(
    b: &OpenApiBroker,
    p: &PropAccount,
    now: DateTime<Utc>,
    boundary: DateTime<Utc>,
) -> Result<Ledger> {
    ensure!(p.history_from <= now, "risk history starts in future");
    // Cash adjustments alter firm references; support them only through an explicit policy migration.
    let mut start = p.history_from;
    while start < now {
        let end = (start + chrono::Duration::days(7)).min(now);
        let flows=b.read(2143,2144,json!({"fromTimestamp":start.timestamp_millis(),"toTimestamp":end.timestamp_millis()})).await?;
        wire::array_shape(&flows, "depositWithdraw")?;
        ensure!(
            wire::array(&flows, "depositWithdraw").is_empty(),
            "cash adjustments require verified account-policy migration"
        );
        start = end;
    }
    let deals = pages(
        b,
        2133,
        2134,
        "deal",
        "executionTimestamp",
        p.history_from,
        now,
    )
    .await?;
    let orders = pages(
        b,
        2175,
        2176,
        "order",
        "utcLastUpdateTimestamp",
        p.history_from,
        now,
    )
    .await?;
    normalize(p, &deals, orders, boundary)
}
pub(super) fn normalize(
    p: &PropAccount,
    deals: &[Value],
    orders: Vec<Value>,
    boundary: DateTime<Utc>,
) -> Result<Ledger> {
    let mut position_requests = BTreeMap::new();
    let request_setups: BTreeMap<String, String> = orders
        .iter()
        .filter_map(|order| {
            Some((
                order["clientOrderId"].as_str()?.to_owned(),
                order["tradeData"]["label"]
                    .as_str()?
                    .strip_prefix("risk:")?
                    .to_owned(),
            ))
        })
        .collect();
    let filled: Vec<_> = deals
        .iter()
        .filter(|d| {
            wire::is_enum(d, "dealStatus", 2, "FILLED")
                || wire::is_enum(d, "dealStatus", 3, "PARTIALLY_FILLED")
        })
        .collect();
    for deal in filled
        .iter()
        .filter(|d| d.get("closePositionDetail").is_none())
    {
        let order_id = wire::integer(deal, "orderId")?;
        let order = orders
            .iter()
            .find(|o| wire::integer(o, "orderId").ok() == Some(order_id))
            .ok_or_else(|| eyre!("opening order attribution missing"))?;
        let id = order["clientOrderId"]
            .as_str()
            .ok_or_else(|| eyre!("unattributed account execution; reconciliation required"))?;
        ensure!(
            request_setups
                .get(id)
                .is_some_and(|setup| !setup.is_empty()),
            "unprotected/manual account execution; reconciliation required"
        );
        let pos = wire::integer(deal, "positionId")?;
        if let Some(previous) = position_requests.insert(pos, id.to_owned()) {
            ensure!(previous == id, "multiple setups share netted position");
        }
    }
    let mut seen = BTreeSet::new();
    let mut events = Vec::new();
    for deal in filled {
        let id = wire::integer(deal, "dealId")?;
        ensure!(
            id > 0 && wire::integer(deal, "filledVolume")? > 0,
            "invalid deal identity/filled volume"
        );
        ensure!(seen.insert(id), "duplicate broker deal");
        let pos = wire::integer(deal, "positionId")?;
        let request = position_requests
            .get(&pos)
            .ok_or_else(|| eyre!("closing deal has no reconstructable opening"))?;
        let setup = request_setups
            .get(request)
            .ok_or_else(|| eyre!("opening order setup identity missing"))?;
        let closed = deal.get("closePositionDetail");
        if let Some(detail) = closed {
            ensure!(
                number(detail, "closedVolume")? == number(deal, "filledVolume")?,
                "closing volume does not match executed volume"
            );
        }
        let signed_fee = |v: Decimal| if p.commission_is_positive_cost { -v } else { v };
        let (volume, pnl) = if let Some(detail) = closed {
            let fee = match p.fee_mode {
                FeeMode::CloseDetailIncludesOpening => money(detail, "commission")?,
                FeeMode::PerDeal => money(deal, "commission")?,
            };
            let conversion_fee = if detail.get("pnlConversionFee").is_some() {
                money(detail, "pnlConversionFee")?.abs()
            } else {
                Decimal::ZERO
            };
            (
                -(number(detail, "closedVolume")?),
                minus(
                    plus(
                        plus(money(detail, "grossProfit")?, money(detail, "swap")?)?,
                        signed_fee(fee),
                    )?,
                    conversion_fee,
                )?,
            )
        } else {
            (
                number(deal, "filledVolume")?,
                match p.fee_mode {
                    FeeMode::PerDeal => signed_fee(money(deal, "commission")?),
                    FeeMode::CloseDetailIncludesOpening => Decimal::ZERO,
                },
            )
        };
        ensure!(volume != Decimal::ZERO, "zero filled deal");
        ensure!(
            closed.is_some() || pnl <= Decimal::ZERO,
            "opening rebates require verified balance-policy support"
        );
        events.push(Execution {
            event_id: id.to_string(),
            setup_id: setup.clone(),
            position_id: pos.to_string(),
            occurred_at: DateTime::from_timestamp_millis(wire::integer(
                deal,
                "executionTimestamp",
            )?)
            .ok_or_else(|| eyre!("invalid execution timestamp"))?,
            volume_delta: divide(volume, Decimal::from(100))?,
            net_pnl: pnl,
        });
    }
    events.sort_by(|a, b| {
        (a.occurred_at, a.event_id.len(), &a.event_id).cmp(&(
            b.occurred_at,
            b.event_id.len(),
            &b.event_id,
        ))
    });
    let mut balance = p.initial_balance;
    let mut high = balance;
    let mut boundary_balance = balance;
    let mut historical_day = p.risk.calendar.day_at(p.history_from)?;
    let mut daily_reference = balance;
    let mut account_breached = false;
    let mut daily_breached = BTreeSet::new();
    for event in &events {
        let event_day = p.risk.calendar.day_at(event.occurred_at)?;
        if event_day != historical_day {
            historical_day = event_day;
            daily_reference = balance;
        }
        balance = plus(balance, event.net_pnl)?;
        high = high.max(balance);
        let drawdown_reference = match p.risk.drawdown_reference {
            adaptive_risk::DrawdownReference::StaticInitialBalance => p.initial_balance,
            _ => high,
        };
        let allowance = if p.risk.trailing_allowance_tracks_reference {
            drawdown_reference
        } else {
            p.initial_balance
        };
        let floor = minus(
            drawdown_reference,
            divide(
                times(allowance, p.risk.internal_account_stop_pct)?,
                Decimal::from(100),
            )?,
        )?;
        account_breached |= balance <= floor;
        let daily_floor = minus(
            daily_reference,
            divide(
                times(daily_reference, p.risk.internal_daily_stop_pct)?,
                Decimal::from(100),
            )?,
        )?;
        if balance <= daily_floor {
            daily_breached.insert(historical_day.start);
        }
        let id = event.event_id.parse::<i64>()?;
        if let Some(detail) = deals
            .iter()
            .find(|d| wire::integer(d, "dealId").ok() == Some(id))
            .and_then(|d| d.get("closePositionDetail"))
        {
            let actual = money(detail, "balance")?;
            ensure!(
                actual == balance,
                "execution ordering/net fees disagree with broker balance checkpoint"
            );
            high = high.max(actual);
        }
        if event.occurred_at < boundary {
            boundary_balance = balance;
        }
    }
    Ok(Ledger {
        events,
        position_requests,
        orders,
        request_setups,
        account_breached,
        daily_breached,
        high,
        boundary_balance,
    })
}
