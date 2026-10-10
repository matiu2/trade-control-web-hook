//! Remove forming bars before caching a provider response.
use candle_model::{BidAskCandles, BidAskDataSource, Candles, DataSource, Granularity, Timestamp};
use color_eyre::Result;
use std::sync::Arc;
use chrono::Utc;

pub(super) struct ClosedSource<D>(pub Arc<D>);
impl<D: BidAskDataSource> DataSource for ClosedSource<D> {
    async fn get_instruments(&self) -> Result<Vec<String>> { self.0.get_instruments().await }
    async fn get_candles(&self, instrument: &str, count: usize, granularity: Granularity) -> Result<Candles> {
        let mut response = self.0.get_candles(instrument, count, granularity).await?;
        response.candles.retain(|c| super::closed(c.timestamp.to_utc(), granularity, Utc::now()));
        Ok(response)
    }
    async fn get_candles_range(&self, instrument: &str, from: Timestamp, to: Timestamp, granularity: Granularity) -> Result<Candles> {
        let mut response = self.0.get_candles_range(instrument, from, to, granularity).await?;
        response.candles.retain(|c| super::closed(c.timestamp.to_utc(), granularity, Utc::now().min(to.to_utc())));
        Ok(response)
    }
}
impl<D: BidAskDataSource> BidAskDataSource for ClosedSource<D> {
    async fn get_candles_bid_ask(&self, instrument: &str, count: usize, granularity: Granularity) -> Result<BidAskCandles> {
        let mut response = self.0.get_candles_bid_ask(instrument, count, granularity).await?;
        response.candles.retain(|c| super::closed(c.timestamp.to_utc(), granularity, Utc::now()));
        Ok(response)
    }
    async fn get_candles_range_bid_ask(&self, instrument: &str, from: Timestamp, to: Timestamp, granularity: Granularity) -> Result<BidAskCandles> {
        let mut response = self.0.get_candles_range_bid_ask(instrument, from, to, granularity).await?;
        response.candles.retain(|c| super::closed(c.timestamp.to_utc(), granularity, Utc::now().min(to.to_utc())));
        Ok(response)
    }
}
