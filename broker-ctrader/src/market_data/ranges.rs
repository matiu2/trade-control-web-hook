use super::*;
use std::collections::BTreeMap;

const LIMIT: usize = 512;
const MAX_TIMESTAMP: i64 = 2_147_483_646_000;
const MAX_PAGES: usize = 10_000;

pub(super) fn timestamp_windows(
    period: NativePeriod,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<(i64, i64)>> {
    ensure!(
        start < end,
        "cTrader native bar range must have positive duration"
    );
    let ceil_millis = |time: DateTime<Utc>| {
        time.timestamp_millis()
            + i64::from(!time.timestamp_subsec_nanos().is_multiple_of(1_000_000))
    };
    let start = ceil_millis(start);
    let end = ceil_millis(end);
    if start == end {
        ensure!(
            start >= 0 && end <= MAX_TIMESTAMP,
            "cTrader bar timestamps must be between 1970 and January 2038"
        );
        return Ok(Vec::new());
    }
    windows(period, start, end)
}

pub(super) fn windows(period: NativePeriod, start: i64, end: i64) -> Result<Vec<(i64, i64)>> {
    ensure!(
        start >= 0 && start < end && end <= MAX_TIMESTAMP,
        "cTrader native bar range must be positive and within 1970 to January 2038"
    );
    let span = (period.minimum_minutes() * 60_000 * 500).min(maximum_span(period));
    let count = (end - start - 1) / span + 1;
    ensure!(
        count <= MAX_PAGES as i64,
        "cTrader bar range exceeds bounded pagination limit; request smaller ranges"
    );
    Ok((0..count)
        .map(|index| {
            let from = start + index * span;
            (from, (from + span).min(end))
        })
        .collect())
}

/// Spotware's official OpenAPI.Net range limits, independent of tick-history
/// limits: <https://github.com/spotware/OpenAPI.Net/blob/master/src/OpenAPI.Net/Helpers/TrendbarsMaximumTime.cs>.
fn maximum_span(period: NativePeriod) -> i64 {
    use NativePeriod::*;
    match period {
        Minute1 | Minute2 | Minute3 | Minute4 | Minute5 => 302_400_000,
        Minute10 | Minute15 | Minute30 | Hour1 => 21_168_000_000,
        Hour4 | Hour12 | Day1 => 31_622_400_000,
        Week1 | Month1 => 158_112_000_000,
    }
}

pub(super) fn next_end(
    bars: &[NativeBar],
    more: bool,
    start: i64,
    end: i64,
) -> Result<Option<i64>> {
    if !more && bars.len() < LIMIT {
        return Ok(None);
    }
    let oldest = bars
        .iter()
        .map(|bar| bar.time.timestamp_millis())
        .min()
        .ok_or_else(|| eyre!("cTrader trendbar pagination hasMore without data"))?;
    if oldest == start && !more {
        return Ok(None);
    }
    ensure!(
        start < oldest && oldest < end,
        "cTrader trendbar pagination cannot progress; refusing incomplete history"
    );
    Ok(Some(oldest))
}

pub(super) async fn read(
    broker: &OpenApiBroker,
    symbol: &InstrumentMetadata,
    period: NativePeriod,
    windows: Vec<(i64, i64)>,
) -> Result<Vec<NativeBar>> {
    read_with(symbol, period, windows, |payload| async move {
        broker.read(2137, 2138, payload).await
    })
    .await
}

pub(super) async fn read_with<F, Fut>(
    symbol: &InstrumentMetadata,
    period: NativePeriod,
    windows: Vec<(i64, i64)>,
    mut fetch: F,
) -> Result<Vec<NativeBar>>
where
    F: FnMut(Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let mut result = BTreeMap::new();
    let mut pages = 0;
    for (start, end) in windows {
        let mut page_end = end;
        loop {
            pages += 1;
            ensure!(
                pages <= MAX_PAGES,
                "cTrader trendbar pagination limit exceeded; refusing incomplete history"
            );
            let response = fetch(json!({"symbolId": symbol.id, "period":period.wire().0, "fromTimestamp":start, "toTimestamp":page_end - 1, "count":LIMIT})).await?;
            let (bars, more) =
                decode::page(&response, symbol.id, period, symbol.digits, start, page_end)?;
            ensure!(
                bars.len() <= LIMIT,
                "cTrader trendbar page exceeds requested count"
            );
            let next = next_end(&bars, more, start, page_end)?;
            for bar in bars {
                if let Some(previous) = result.insert(bar.time, bar.clone()) {
                    ensure!(
                        previous == bar,
                        "cTrader returned conflicting bars at the same timestamp"
                    );
                }
            }
            let Some(next) = next else { break };
            page_end = next;
        }
    }
    tracing::debug!(symbol = %symbol.name, ?period, bars = result.len(), pages, "cTrader native BID history loaded");
    Ok(result.into_values().collect())
}
