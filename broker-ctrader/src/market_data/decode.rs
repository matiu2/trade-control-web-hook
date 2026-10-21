use super::*;

pub(super) fn bar(row: &Value, digits: u32, period: NativePeriod) -> Result<NativeBar> {
    ensure!(digits <= 8, "invalid cTrader bar price precision");
    if row.get("period").is_some() {
        ensure!(
            wire::is_enum(row, "period", period.wire().0, period.wire().1),
            "cTrader trendbar has a different period"
        );
    }
    let minute = wire::integer(row, "utcTimestampInMinutes")?;
    ensure!(
        minute >= 0 && minute <= i64::from(u32::MAX),
        "invalid cTrader trendbar timestamp"
    );
    let time = DateTime::from_timestamp(minute * 60, 0)
        .ok_or_else(|| eyre!("invalid cTrader trendbar time"))?;
    let low = wire::integer(row, "low")?;
    ensure!(low > 0, "invalid cTrader trendbar low price");
    let open = relative(row, "deltaOpen", low)?;
    let high = relative(row, "deltaHigh", low)?;
    let close = relative(row, "deltaClose", low)?;
    ensure!(
        open <= high && close <= high,
        "invalid cTrader trendbar OHLC geometry"
    );
    let volume = u64::try_from(wire::integer(row, "volume")?)
        .map_err(|_| eyre!("negative cTrader trendbar tick volume"))?;
    let scale = 10f64.powi(digits as i32);
    let price = |raw: i64| ((raw as f64 / 100000.0) * scale).round() / scale;
    let bar = NativeBar {
        time,
        open: price(open),
        high: price(high),
        low: price(low),
        close: price(close),
        volume,
    };
    ensure!(
        [bar.open, bar.high, bar.low, bar.close]
            .iter()
            .all(|price| price.is_finite() && *price > 0.0),
        "invalid cTrader trendbar price"
    );
    Ok(bar)
}

fn relative(row: &Value, field: &str, low: i64) -> Result<i64> {
    let delta = if row.get(field).is_some() {
        wire::integer(row, field)?
    } else {
        0
    };
    ensure!(delta >= 0, "negative cTrader trendbar {field}");
    low.checked_add(delta)
        .ok_or_else(|| eyre!("cTrader trendbar price overflow"))
}

pub(super) fn page(
    response: &Value,
    symbol: i64,
    period: NativePeriod,
    digits: u32,
    start: i64,
    end: i64,
) -> Result<(Vec<NativeBar>, bool)> {
    ensure!(
        wire::is_enum(response, "period", period.wire().0, period.wire().1),
        "cTrader trendbar response has a different period"
    );
    if response.get("symbolId").is_some() {
        ensure!(
            wire::integer(response, "symbolId")? == symbol,
            "cTrader trendbar response belongs to another symbol"
        );
    }
    wire::array_shape(response, "trendbar")?;
    let bars = wire::array(response, "trendbar")
        .iter()
        .map(|row| bar(row, digits, period))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        bars.iter()
            .all(|bar| (start..end).contains(&bar.time.timestamp_millis())),
        "cTrader trendbar response lies outside requested range"
    );
    let more = response
        .get("hasMore")
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| eyre!("invalid cTrader trendbar hasMore flag"))
        })
        .transpose()?
        .unwrap_or(false);
    Ok((bars, more))
}
