//! Split failed requests around audited whole minutes; only strict valid ranges reach the cache.
use super::{Minute, audit::Audit};
use candle_model::BidAskCandleData;
use chrono::{DateTime, Utc};
use color_eyre::{
    Result,
    eyre::{Context, ensure},
};
use std::future::Future;

pub(crate) async fn collect<F, Fut>(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    symbol: &str,
    symbol_id: i64,
    audit: &mut Audit,
    mut fetch: F,
) -> Result<Vec<BidAskCandleData>>
where
    F: FnMut(DateTime<Utc>, DateTime<Utc>) -> Fut,
    Fut: Future<Output = Result<Vec<BidAskCandleData>>>,
{
    ensure!(
        from < to && aligned(from) && aligned(to),
        "quarantine collection requires whole closed minutes"
    );
    let known = audit.minutes(symbol, symbol_id)?;
    let mut pending = segments(from, to, &known);
    pending.reverse();
    let mut candles = Vec::new();
    while let Some((left, right)) = pending.pop() {
        match fetch(left, right).await {
            Ok(rows) => {
                ensure!(
                    rows.iter().all(|r| {
                        let at = r.timestamp.with_timezone(&Utc);
                        at >= left && at < right && aligned(at)
                    }),
                    "strict cache returned a candle outside its requested segment"
                );
                candles.extend(rows);
            }
            Err(error) => {
                let Some(crossed) =
                    error.downcast_ref::<ctrader_data_source::CrossedHistoricalQuote>()
                else {
                    return Err(error);
                };
                let minute = Minute::from_error(symbol, symbol_id, crossed)?;
                ensure!(
                    minute.observed_at_utc >= left
                        && minute.observed_at_utc < right
                        && minute.from_utc >= left
                        && minute.to_utc <= right,
                    "crossed quote lies outside the strict request"
                );
                audit.record(minute.clone()).wrap_err_with(||format!("refused calibration exclusion for {symbol} [{},{}), observation={} bid={} ask={}",minute.from_utc,minute.to_utc,minute.observed_at_utc,minute.bid,minute.ask))?;
                tracing::warn!(symbol,from=%minute.from_utc,to=%minute.to_utc,bid=minute.bid,ask=minute.ask,"calibration excludes one crossed M1 minute; durable audit recorded; runtime source remains strict");
                if minute.to_utc < right {
                    pending.push((minute.to_utc, right));
                }
                if left < minute.from_utc {
                    pending.push((left, minute.from_utc));
                }
            }
        }
    }
    candles.sort_by_key(|r| r.timestamp);
    ensure!(
        candles.windows(2).all(|r| r[0].timestamp < r[1].timestamp),
        "strict calibration segments produced duplicate candles"
    );
    Ok(candles)
}
fn aligned(time: DateTime<Utc>) -> bool {
    time.timestamp().rem_euclid(60) == 0 && time.timestamp_subsec_nanos() == 0
}
fn segments(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    known: &[Minute],
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut rows = Vec::new();
    let mut cursor = from;
    for minute in known.iter().filter(|r| r.to_utc > from && r.from_utc < to) {
        if cursor < minute.from_utc {
            rows.push((cursor, minute.from_utc.min(to)));
        }
        cursor = cursor.max(minute.to_utc);
    }
    if cursor < to {
        rows.push((cursor, to));
    }
    rows
}
