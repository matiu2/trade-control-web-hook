use candle_model::{BidAskCandleData, BidAskDataSource};
use chrono::{DateTime, Utc};
use mt5_data_source::tick_source::TickDataSource;
use trade_control_core::broker::{BidAskCandle, CandleError, Granularity};

pub(crate) async fn read(
    source: &TickDataSource,
    symbol: &str,
    gran: Granularity,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Vec<BidAskCandle>, CandleError> {
    if since >= now {
        return Err(CandleError::BadRange);
    }
    let gran = match gran {
        Granularity::M1 => candle_model::Granularity::OneMinute,
        Granularity::M5 => candle_model::Granularity::FiveMinutes,
        Granularity::M15 => candle_model::Granularity::FifteenMinutes,
        Granularity::H1 => candle_model::Granularity::OneHour,
        Granularity::H4 => candle_model::Granularity::FourHours,
        Granularity::D1 => candle_model::Granularity::OneDay,
    };
    let bars = source
        .get_candles_range_bid_ask(symbol, since.fixed_offset(), now.fixed_offset(), gran)
        .await
        .map_err(|error| {
            tracing::warn!(symbol, %error, "MT5 historical tick candles unavailable");
            CandleError::Transient
        })?;
    Ok(bars
        .candles
        .iter()
        .filter(|c| c.timestamp > since)
        .map(to_core)
        .collect())
}
pub(crate) fn to_core(c: &BidAskCandleData) -> BidAskCandle {
    BidAskCandle {
        time: c.timestamp.with_timezone(&Utc),
        o: c.open,
        h: c.high,
        l: c.low,
        c: c.close,
        bid_o: c.bid_open,
        bid_h: c.bid_high,
        bid_l: c.bid_low,
        bid_c: c.bid_close,
        ask_o: c.ask_open,
        ask_h: c.ask_high,
        ask_l: c.ask_low,
        ask_c: c.ask_close,
    }
}
