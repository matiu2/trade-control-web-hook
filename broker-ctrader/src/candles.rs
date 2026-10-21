//! Tick books must be paired before OHLC aggregation: mean(book highs) is not a MID high.
use super::*;
use std::collections::BTreeMap;
use wire::integer;
type Tick = (i64, f64);
const WEEK: i64 = 604_800_000;

pub(crate) fn decode(rows: &Value) -> Result<Vec<Tick>> {
    let rows = rows
        .as_array()
        .ok_or_else(|| eyre!("cTrader tickData is not an array"))?;
    let mut time = 0i64;
    let mut price = 0i64;
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let dt = integer(row, "timestamp")?;
            let dp = integer(row, "tick")?;
            if index == 0 {
                time = dt;
                price = dp;
            } else {
                ensure!(dt <= 0, "historical tick timestamps must descend");
                time = time
                    .checked_add(dt)
                    .ok_or_else(|| eyre!("tick time overflow"))?;
                price = price
                    .checked_add(dp)
                    .ok_or_else(|| eyre!("tick price overflow"))?;
            }
            ensure!(time >= 0 && price > 0, "invalid decoded cTrader tick");
            Ok((time, price as f64 / 100000.0))
        })
        .collect()
}
async fn page(
    broker: &OpenApiBroker,
    symbol: i64,
    side: i64,
    start: i64,
    end: i64,
) -> Result<(Vec<Tick>, bool)> {
    let response = broker
        .read(
            2145,
            2146,
            json!({"symbolId":symbol,"type":side,"fromTimestamp":start,"toTimestamp":end}),
        )
        .await?;
    let rows = response
        .get("tickData")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let more = response["hasMore"]
        .as_bool()
        .ok_or_else(|| eyre!("tick history omitted hasMore"))?;
    Ok((decode(&rows)?, more))
}
async fn ticks(
    broker: &OpenApiBroker,
    symbol: i64,
    side: i64,
    start: i64,
    end: i64,
) -> Result<Vec<Tick>> {
    let mut result = Vec::new();
    let mut cursor = start;
    while cursor < end {
        let window_end = (cursor + WEEK).min(end);
        let mut page_end = window_end;
        for count in 0..2000 {
            let (rows, more) = page(broker, symbol, side, cursor, page_end).await?;
            ensure!(
                rows.iter().all(|(t, _)| *t >= cursor && *t <= page_end),
                "cTrader returned ticks outside requested range"
            );
            let oldest = rows.last().map(|r| r.0);
            result.extend(rows);
            if !more {
                break;
            }
            let oldest = oldest.ok_or_else(|| eyre!("tick pagination hasMore without data"))?;
            ensure!(
                oldest < page_end,
                "cTrader tick pagination cannot progress at same-millisecond boundary; refusing incomplete history"
            );
            ensure!(
                count < 1999,
                "cTrader tick history exceeded pagination limit"
            );
            page_end = oldest;
        }
        cursor = window_end;
    }
    result.sort_by_key(|r| r.0);
    result.dedup_by(|a, b| a == b);
    ensure!(
        !result
            .windows(2)
            .any(|w| w[0].0 == w[1].0 && w[0].1 != w[1].1),
        "cTrader history has multiple same-side prices at one millisecond; quote order unavailable"
    );
    Ok(result)
}
async fn seed(broker: &OpenApiBroker, symbol: i64, side: i64, before: i64) -> Result<Tick> {
    let (rows, _) = page(broker, symbol, side, (before - WEEK).max(0), before).await?;
    rows.first().copied().ok_or_else(|| {
        eyre!("cTrader historical quote side {side} has no seed within preceding week")
    })
}
struct Bar {
    time: i64,
    book: BidAskCandle,
}
impl Bar {
    fn new(time: i64, bid: f64, ask: f64) -> Result<Self> {
        let mid = (bid + ask) / 2.0;
        Ok(Self {
            time,
            book: BidAskCandle {
                time: DateTime::from_timestamp_millis(time)
                    .ok_or_else(|| eyre!("invalid candle time"))?,
                o: mid,
                h: mid,
                l: mid,
                c: mid,
                bid_o: bid,
                bid_h: bid,
                bid_l: bid,
                bid_c: bid,
                ask_o: ask,
                ask_h: ask,
                ask_l: ask,
                ask_c: ask,
            },
        })
    }
    fn update(&mut self, bid: f64, ask: f64) {
        let mid = (bid + ask) / 2.0;
        let b = &mut self.book;
        b.h = b.h.max(mid);
        b.l = b.l.min(mid);
        b.c = mid;
        b.bid_h = b.bid_h.max(bid);
        b.bid_l = b.bid_l.min(bid);
        b.bid_c = bid;
        b.ask_h = b.ask_h.max(ask);
        b.ask_l = b.ask_l.min(ask);
        b.ask_c = ask;
    }
}
fn validate_side(ticks: &[Tick]) -> Result<()> {
    ensure!(
        ticks
            .iter()
            .all(|(t, p)| *t >= 0 && p.is_finite() && *p > 0.0),
        "invalid historical tick values"
    );
    ensure!(
        !ticks
            .windows(2)
            .any(|w| w[0].0 > w[1].0 || (w[0].0 == w[1].0 && w[0].1 != w[1].1)),
        "cTrader multiple same-side prices at one millisecond or unordered history; cannot resolve quote path"
    );
    Ok(())
}
pub(crate) fn aggregate(
    bids: &[Tick],
    asks: &[Tick],
    seconds: i64,
    start: i64,
    end: i64,
) -> Result<Vec<BidAskCandle>> {
    ensure!(
        !bids.is_empty() && !asks.is_empty(),
        "paired bid/ask tick history unavailable"
    );
    validate_side(bids)?;
    validate_side(asks)?;
    let mut events: BTreeMap<i64, (Option<f64>, Option<f64>)> = BTreeMap::new();
    bids.iter()
        .for_each(|(t, p)| events.entry(*t).or_default().0 = Some(*p));
    asks.iter()
        .for_each(|(t, p)| events.entry(*t).or_default().1 = Some(*p));
    let mut bid = None;
    let mut ask = None;
    let mut current: Option<Bar> = None;
    let mut result = Vec::new();
    for (time, (new_bid, new_ask)) in events {
        if time >= end {
            break;
        }
        // API timestamps resolve milliseconds. Apply both books atomically at one
        // timestamp: their arrival order is not known, so do not invent intermediate mids.
        bid = new_bid.or(bid);
        ask = new_ask.or(ask);
        if time < start {
            continue;
        }
        let (bid, ask) = bid.zip(ask).ok_or_else(|| {
            eyre!("both historical quote sides must be seeded before candle start")
        })?;
        ensure!(
            bid.is_finite() && ask.is_finite() && bid > 0.0 && ask >= bid,
            "invalid crossed historical cTrader quote"
        );
        let open = time.div_euclid(seconds * 1000) * (seconds * 1000);
        if current.as_ref().is_none_or(|bar| bar.time != open) {
            if let Some(bar) = current.take() {
                result.push(bar.book);
            }
            current = Some(Bar::new(open, bid, ask)?);
        } else if let Some(bar) = current.as_mut() {
            bar.update(bid, ask);
        }
    }
    if let Some(bar) = current {
        result.push(bar.book);
    }
    Ok(result)
}
pub(crate) async fn read(
    broker: &OpenApiBroker,
    instrument: &str,
    gran: Granularity,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> std::result::Result<Vec<BidAskCandle>, CandleError> {
    if since >= now {
        return Err(CandleError::BadRange);
    }
    let seconds = gran.seconds();
    let span = seconds * 1000;
    let start = since.timestamp_millis().div_euclid(span) * span + span;
    let end = now.timestamp_millis().div_euclid(span) * span;
    if start >= end {
        return Ok(Vec::new());
    }
    let fetch = async {
        let symbol = metadata::instrument_metadata_id(broker, instrument).await?;
        let bid_seed = seed(broker, symbol, 1, start - 1).await?;
        let ask_seed = seed(broker, symbol, 2, start - 1).await?;
        let mut bids = ticks(broker, symbol, 1, start, end - 1).await?;
        let mut asks = ticks(broker, symbol, 2, start, end - 1).await?;
        bids.insert(0, bid_seed);
        asks.insert(0, ask_seed);
        aggregate(&bids, &asks, seconds, start, end)
    };
    fetch.await.map_err(|error| {
        tracing::warn!(%error,instrument,"cTrader paired candle history unavailable");
        CandleError::Unavailable(format!("cTrader {instrument}: {error}"))
    })
}
