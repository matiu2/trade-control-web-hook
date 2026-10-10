use super::*;
use candle_cache::storage::{StorageBackend, memory::MemoryStorage};
use candle_model::{BidAskCandles, Candles, DataSource, Timestamp};
use chrono::Duration;
use std::sync::atomic::{AtomicUsize, Ordering};

struct FakeSource {
    calls: AtomicUsize,
    bars: Vec<BidAskCandleData>,
}
impl DataSource for FakeSource {
    async fn get_instruments(&self) -> Result<Vec<String>> {
        Ok(vec!["EUR_USD".into()])
    }
    async fn get_candles(&self, _: &str, _: usize, _: Frame) -> Result<Candles> {
        Err(eyre!("range path required"))
    }
    async fn get_candles_range(
        &self,
        _: &str,
        _: Timestamp,
        _: Timestamp,
        _: Frame,
    ) -> Result<Candles> {
        Err(eyre!("paired cache path required"))
    }
}
impl BidAskDataSource for FakeSource {
    async fn get_candles_bid_ask(&self, _: &str, _: usize, _: Frame) -> Result<BidAskCandles> {
        Err(eyre!("range path required"))
    }
    async fn get_candles_range_bid_ask(
        &self,
        instrument: &str,
        _: Timestamp,
        _: Timestamp,
        granularity: Frame,
    ) -> Result<BidAskCandles> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(BidAskCandles {
            instrument_name: instrument.into(),
            granularity,
            candles: self.bars.clone(),
        })
    }
}
fn bar(stamp: &str) -> BidAskCandleData {
    BidAskCandleData {
        timestamp: stamp.parse().unwrap(),
        volume: 10.0,
        open: 1.1,
        high: 1.2,
        low: 1.0,
        close: 1.15,
        bid_open: 1.09,
        bid_high: 1.19,
        bid_low: 0.99,
        bid_close: 1.14,
        ask_open: 1.11,
        ask_high: 1.21,
        ask_low: 1.01,
        ask_close: 1.16,
    }
}
fn logging() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}
#[tokio::test]
async fn broker_miss_fills_shared_cache_then_hits_without_a_broker_and_keeps_real_books() {
    logging();
    let as_of = Utc::now();
    let forming_start =
        candle_cache::cache_key::CacheKey::new("EUR_USD", Frame::OneHour, as_of.fixed_offset())
            .timestamp_bucket;
    let since = forming_start - Duration::hours(2);
    let expected = forming_start - Duration::hours(1);
    let provider = Arc::new(FakeSource {
        calls: AtomicUsize::new(0),
        bars: vec![
            bar(&since.to_rfc3339()),
            bar(&expected.to_rfc3339()),
            bar(&forming_start.to_rfc3339()),
        ],
    });
    let storage: Arc<dyn StorageBackend> = Arc::new(MemoryStorage::new());
    let cache = CacheClient::with_storage(
        CacheConfig::default()
            .with_cache_dir("candle_cache_oanda")
            .with_background_eviction(false),
        source::ClosedSource(provider.clone()),
        storage.clone(),
    )
    .await
    .unwrap();
    let puller = Puller {
        source: provider.clone(),
        namespace: "candle_cache_oanda".into(),
        cache: OnceCell::new(),
    };
    puller.cache.set(cache).ok().unwrap();
    let first = puller
        .bidask("EUR_USD", Granularity::H1, since, as_of)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].time, expected);
    assert_eq!((first[0].bid_c, first[0].ask_c), (1.14, 1.16));
    let calls = provider.calls.load(Ordering::SeqCst);
    assert!(calls > 0);
    assert_eq!(
        puller
            .mid("EUR_USD", Granularity::H1, since, as_of)
            .await
            .unwrap()[0]
            .c,
        1.15
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), calls);
    let forming = candle_cache::cache_key::bid_ask_cache_key(
        "EUR_USD",
        Frame::OneHour,
        forming_start.fixed_offset(),
    );
    assert!(
        storage
            .get_bid_ask(&forming)
            .await
            .unwrap()
            .is_none_or(|entry| entry.candles.is_empty())
    );
}
#[test]
fn closed_boundary_watermark_and_sorting_are_preserved() {
    logging();
    let since = "2026-10-09T00:00:00Z".parse().unwrap();
    let to = "2026-10-09T02:00:00Z".parse().unwrap();
    let bars = [
        bar("2026-10-09T02:00:00Z"),
        bar("2026-10-09T01:00:00Z"),
        bar("2026-10-09T00:00:00Z"),
        bar("2026-10-09T01:00:00Z"),
    ];
    assert_eq!(
        closed_window("EUR_USD", &bars, Frame::OneHour, since, to).len(),
        1
    );
    assert!(!closed(
        "EUR_USD",
        to,
        Frame::OneHour,
        to + Duration::minutes(59)
    ));
    assert!(closed(
        "EUR_USD",
        to,
        Frame::OneHour,
        to + Duration::hours(1)
    ));
}

#[test]
fn daily_closure_follows_the_session_across_dst() {
    logging();
    let start = "2026-03-07T22:00:00Z".parse().unwrap();
    let end = "2026-03-08T21:00:00Z".parse().unwrap();
    assert!(!closed(
        "EUR_USD",
        start,
        Frame::OneDay,
        end - Duration::seconds(1)
    ));
    assert!(closed("EUR_USD", start, Frame::OneDay, end));
}
