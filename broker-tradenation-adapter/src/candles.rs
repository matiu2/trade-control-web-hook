//! The authenticated TradeNation session supplies only candle-cache misses.
use broker_tradenation::TradeNationBroker;
use candle_model::{
    BidAskCandleData, BidAskCandles, BidAskDataSource, CandleData, Candles, DataSource,
    Granularity, Timestamp,
};
use chrono::{Duration, Utc};
use color_eyre::{Result, eyre::eyre};
use std::{collections::BTreeMap, sync::Arc};
use tradenation_api::ohlcv::PriceType;

pub(super) struct Source(pub Arc<TradeNationBroker>);
impl Source {
    async fn range(
        &self,
        instrument: &str,
        from: Timestamp,
        to: Timestamp,
        granularity: Granularity,
    ) -> Result<BidAskCandles> {
        let core = match granularity {
            Granularity::OneMinute => trade_control_core::broker::Granularity::M1,
            Granularity::FiveMinutes => trade_control_core::broker::Granularity::M5,
            Granularity::FifteenMinutes => trade_control_core::broker::Granularity::M15,
            Granularity::OneHour => trade_control_core::broker::Granularity::H1,
            Granularity::FourHours => trade_control_core::broker::Granularity::H4,
            Granularity::OneDay => trade_control_core::broker::Granularity::D1,
            _ => {
                return Err(eyre!(
                    "unsupported TradeNation engine timeframe {granularity}"
                ));
            }
        };
        let market =
            tradenation_api::resolve_market(self.0.client(), self.0.session(), instrument).await?;
        let mut candles = BTreeMap::new();
        for (count, end) in super::chunk_windows(core, from.to_utc(), to.to_utc()) {
            let fetch = |price| {
                tradenation_api::aggregation::get_candles_range_aggregated_on(
                    self.0.client(),
                    market.market_id,
                    granularity,
                    price,
                    count,
                    end,
                    super::session_anchor::session_anchor(instrument),
                )
            };
            let mid = fetch(PriceType::Mid).await?;
            let bid: BTreeMap<_, _> = fetch(PriceType::Bid)
                .await?
                .into_iter()
                .map(|c| (c.timestamp, c))
                .collect();
            let ask: BTreeMap<_, _> = fetch(PriceType::Ask)
                .await?
                .into_iter()
                .map(|c| (c.timestamp, c))
                .collect();
            for m in &mid {
                if let (Some(b), Some(a)) = (bid.get(&m.timestamp), ask.get(&m.timestamp))
                    && m.timestamp >= from
                    && m.timestamp < to
                {
                    candles.insert(m.timestamp, BidAskCandleData::from_bid_mid_ask(b, m, a));
                }
            }
        }
        Ok(BidAskCandles {
            instrument_name: instrument.into(),
            granularity,
            candles: candles.into_values().collect(),
        })
    }
}
fn mid(data: BidAskCandles) -> Result<Candles> {
    Ok(Candles {
        instrument_name: data.instrument_name,
        granularity: data.granularity,
        candles: data
            .candles
            .iter()
            .map(|c| CandleData::new(c.open, c.high, c.low, c.close, c.volume, c.timestamp))
            .collect::<Result<_>>()?,
    })
}
impl DataSource for Source {
    async fn get_instruments(&self) -> Result<Vec<String>> {
        Err(eyre!("use instrument-lookup for the TradeNation catalogue"))
    }
    async fn get_candles(
        &self,
        instrument: &str,
        count: usize,
        granularity: Granularity,
    ) -> Result<Candles> {
        mid(self
            .get_candles_bid_ask(instrument, count, granularity)
            .await?)
    }
    async fn get_candles_range(
        &self,
        instrument: &str,
        from: Timestamp,
        to: Timestamp,
        granularity: Granularity,
    ) -> Result<Candles> {
        mid(self.range(instrument, from, to, granularity).await?)
    }
}
impl BidAskDataSource for Source {
    async fn get_candles_bid_ask(
        &self,
        instrument: &str,
        count: usize,
        granularity: Granularity,
    ) -> Result<BidAskCandles> {
        let to = Utc::now().fixed_offset();
        let from =
            to - Duration::seconds((count as i64 + 3) * granularity.duration_seconds() as i64);
        let mut data = self.range(instrument, from, to, granularity).await?;
        let skip = data.candles.len().saturating_sub(count);
        data.candles.drain(..skip);
        Ok(data)
    }
    async fn get_candles_range_bid_ask(
        &self,
        instrument: &str,
        from: Timestamp,
        to: Timestamp,
        granularity: Granularity,
    ) -> Result<BidAskCandles> {
        self.range(instrument, from, to, granularity).await
    }
}
