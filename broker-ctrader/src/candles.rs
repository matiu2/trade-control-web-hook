//! The engine and replay read the same verified paired cache, preserving watermarks.
use super::*;
type CachedRead<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<BidAskCandle>>> + Send + 'a>>;

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
    let from = since
        .checked_add_signed(chrono::Duration::nanoseconds(1))
        .ok_or(CandleError::BadRange)?;
    // Erase the cache's nested future here so the cron engine does not carry
    // protocol, paging and storage future layouts through every broker branch.
    let fetch: CachedRead<'_> = Box::pin(async {
        let feed = broker.shared.candles().await?;
        let data = feed
            .range_bid_ask(
                instrument,
                from.fixed_offset(),
                now.fixed_offset(),
                model_granularity(gran),
            )
            .await?;
        Ok::<_, color_eyre::Report>(
            data.candles
                .into_iter()
                .map(to_engine)
                .filter(|bar| {
                    bar.time > since && bar.time + chrono::Duration::seconds(gran.seconds()) <= now
                })
                .collect(),
        )
    });
    fetch.await.map_err(|error| {
        tracing::warn!(%error, instrument, "cTrader paired cache unavailable");
        CandleError::Unavailable(format!("cTrader {instrument}: {error}"))
    })
}
fn model_granularity(gran: Granularity) -> candle_model::Granularity {
    match gran {
        Granularity::M1 => candle_model::Granularity::OneMinute,
        Granularity::M5 => candle_model::Granularity::FiveMinutes,
        Granularity::M15 => candle_model::Granularity::FifteenMinutes,
        Granularity::H1 => candle_model::Granularity::OneHour,
        Granularity::H4 => candle_model::Granularity::FourHours,
        Granularity::D1 => candle_model::Granularity::OneDay,
    }
}
fn to_engine(bar: candle_model::BidAskCandleData) -> BidAskCandle {
    BidAskCandle {
        time: bar.timestamp.to_utc(),
        o: bar.open,
        h: bar.high,
        l: bar.low,
        c: bar.close,
        bid_o: bar.bid_open,
        bid_h: bar.bid_high,
        bid_l: bar.bid_low,
        bid_c: bar.bid_close,
        ask_o: bar.ask_open,
        ask_h: bar.ask_high,
        ask_l: bar.ask_low,
        ask_c: bar.ask_close,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn engine_mapping_preserves_real_mid_and_books() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
        let input = candle_model::BidAskCandleData {
            timestamp: "2024-01-01T00:00:00Z".parse().unwrap(),
            volume: 10.0,
            open: 1.2,
            high: 1.4,
            low: 1.0,
            close: 1.3,
            bid_open: 1.1,
            bid_high: 1.3,
            bid_low: 0.9,
            bid_close: 1.2,
            ask_open: 1.3,
            ask_high: 1.5,
            ask_low: 1.1,
            ask_close: 1.4,
        };
        let result = to_engine(input);
        assert_eq!(result.h, 1.4);
        assert_eq!(result.ask_c, 1.4);
        assert_eq!(result.bid_c, 1.2);
    }
}
