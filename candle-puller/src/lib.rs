//! Closed candle reads for the worker use the same candle-cache as chart/replay.
mod source;
mod storage;
#[cfg(test)]
mod tests;

use candle_cache::{CacheClient, CacheConfig};
use candle_model::{BidAskCandleData, BidAskDataSource, Granularity as Frame};
use chrono::{DateTime, Utc};
use color_eyre::{Result, eyre::eyre};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::OnceCell;
use trade_control_core::broker::{BidAskCandle, Candle, CandleError, Granularity};

pub struct Puller<D: BidAskDataSource> {
    source: Arc<D>,
    namespace: String,
    cache: OnceCell<CacheClient<source::ClosedSource<D>>>,
}
impl<D: BidAskDataSource> Puller<D> {
    pub fn new(namespace: &str, source: D) -> Self {
        Self {
            source: Arc::new(source),
            namespace: namespace.into(),
            cache: OnceCell::new(),
        }
    }
    async fn cache(&self) -> Result<&CacheClient<source::ClosedSource<D>>> {
        self.cache
            .get_or_try_init(|| async {
                let base = std::env::var_os("HOME").ok_or_else(|| {
                    eyre!("HOME is required for the shared candle-cache location")
                })?;
                let config = CacheConfig::default()
                    .with_cache_dir(PathBuf::from(base).join(".cache").join(&self.namespace))
                    .with_background_eviction(false);
                let storage = storage::open(&config.cache_dir).await?;
                Ok(CacheClient::with_storage(
                    config,
                    source::ClosedSource(self.source.clone()),
                    storage,
                )
                .await?)
            })
            .await
    }
    pub async fn bidask(
        &self,
        instrument: &str,
        granularity: Granularity,
        since: DateTime<Utc>,
        as_of: DateTime<Utc>,
    ) -> std::result::Result<Vec<BidAskCandle>, CandleError> {
        if since >= as_of {
            return Err(CandleError::BadRange);
        }
        let frame = frame(granularity);
        // CacheClient widens partial ends to include a forming bucket. The
        // worker asks for closed bars, so exclude that bucket before lookup.
        let end = candle_cache::cache_key::CacheKey::new(instrument, frame, as_of.fixed_offset())
            .timestamp_bucket;
        if end <= since {
            return Ok(Vec::new());
        }
        let bars = async {
            self.cache()
                .await?
                .get_candles_range_bid_ask(
                    instrument,
                    since.fixed_offset(),
                    end.fixed_offset(),
                    frame,
                )
                .await
                .map_err(Into::into)
        }
        .await
        .map_err(|error: color_eyre::Report| {
            tracing::warn!(instrument, %error, "shared candle-cache read failed");
            CandleError::Transient
        })?;
        Ok(closed_window(
            instrument,
            &bars.candles,
            frame,
            since,
            as_of,
        ))
    }
    pub async fn mid(
        &self,
        instrument: &str,
        granularity: Granularity,
        since: DateTime<Utc>,
        as_of: DateTime<Utc>,
    ) -> std::result::Result<Vec<Candle>, CandleError> {
        Ok(self
            .bidask(instrument, granularity, since, as_of)
            .await?
            .iter()
            .map(BidAskCandle::mid)
            .collect())
    }
}
fn frame(granularity: Granularity) -> Frame {
    match granularity {
        Granularity::M1 => Frame::OneMinute,
        Granularity::M5 => Frame::FiveMinutes,
        Granularity::M15 => Frame::FifteenMinutes,
        Granularity::H1 => Frame::OneHour,
        Granularity::H4 => Frame::FourHours,
        Granularity::D1 => Frame::OneDay,
    }
}
fn closed(instrument: &str, stamp: DateTime<Utc>, frame: Frame, as_of: DateTime<Utc>) -> bool {
    let anchor = candle_cache::instrument_anchor::anchor_for(instrument);
    candle_cache::empty_store::bucket_end_on(stamp, frame, anchor) <= as_of
}
fn closed_window(
    instrument: &str,
    bars: &[BidAskCandleData],
    frame: Frame,
    since: DateTime<Utc>,
    as_of: DateTime<Utc>,
) -> Vec<BidAskCandle> {
    let mut candles: Vec<_> = bars
        .iter()
        .filter(|c| c.timestamp > since && closed(instrument, c.timestamp.to_utc(), frame, as_of))
        .map(|c| BidAskCandle {
            time: c.timestamp.to_utc(),
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
        })
        .collect();
    candles.sort_by_key(|c| c.time);
    candles.dedup_by_key(|c| c.time);
    candles
}
