//! One persistent account session per environment, shared across worker ticks.
use super::*;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify, OnceCell, RwLock};

pub(crate) struct Shared {
    pub client: Client,
    risk_revision: std::sync::atomic::AtomicU64,
    risk_healthy: std::sync::atomic::AtomicBool,
    pub(crate) risk_monitor_started: std::sync::atomic::AtomicBool,
    config: Config,
    candles: OnceCell<Arc<candle_cache::ctrader::CachedFeed>>,
    quotes: RwLock<HashMap<i64, Book>>,
    subscriptions: Mutex<HashSet<i64>>,
    changed: Notify,
}
#[derive(Default)]
struct Book {
    bid: Option<(f64, Instant, i64)>,
    ask: Option<(f64, Instant, i64)>,
}
struct Slot {
    config: Config,
    session: Arc<Shared>,
}
static SESSIONS: OnceLock<Mutex<Vec<Slot>>> = OnceLock::new();
fn same(a: &Config, b: &Config) -> bool {
    a.account_id == b.account_id
        && a.environment == b.environment
        && a.client_id == b.client_id
        && a.client_secret == b.client_secret
        && a.access_token == b.access_token
}
pub(crate) async fn acquire(config: Config) -> Result<Arc<Shared>> {
    let mut slots = SESSIONS.get_or_init(|| Mutex::new(Vec::new())).lock().await;
    if let Some(slot) = slots
        .iter()
        .find(|slot| same(&slot.config, &config) && slot.session.risk_healthy())
    {
        return Ok(slot.session.clone());
    }
    if let Some(index) = slots
        .iter()
        .position(|slot| slot.config.environment == config.environment)
    {
        ensure!(
            slots[index].config.account_id == config.account_id
                || slots[index].session.client.is_closed(),
            "cTrader currently supports one active configured account per demo/live environment; restart after changing account"
        );
        let old = slots.remove(index);
        old.session.client.close().await;
    }
    let client = Client::connect(config.clone()).await?;
    let mut events = client.events();
    let shared = Arc::new(Shared {
        client,
        risk_revision: std::sync::atomic::AtomicU64::new(0),
        risk_healthy: std::sync::atomic::AtomicBool::new(true),
        risk_monitor_started: std::sync::atomic::AtomicBool::new(false),
        config: config.clone(),
        candles: OnceCell::new(),
        quotes: RwLock::new(HashMap::new()),
        subscriptions: Mutex::new(HashSet::new()),
        changed: Notify::new(),
    });
    let weak = Arc::downgrade(&shared);
    tokio::spawn(async move {
        loop {
            let event = tokio::select! {event=events.recv()=>event,_=tokio::time::sleep(Duration::from_secs(5))=>{if weak.upgrade().is_none_or(|session|session.client.is_closed()) {break;}continue;}};
            let Some(shared) = weak.upgrade() else {
                break;
            };
            let event = match event {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    shared
                        .risk_healthy
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    shared.quotes.write().await.clear();
                    continue;
                }
                Err(_) => break,
            };
            if matches!(event.payload_type, 2107 | 2123 | 2126 | 2132) {
                shared
                    .risk_revision
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            if event.payload_type != 2131
                || wire::integer(&event.payload, "ctidTraderAccountId").ok()
                    != Some(shared.client.account_id())
            {
                continue;
            }
            let (Ok(symbol), Ok(time)) = (
                wire::integer(&event.payload, "symbolId"),
                wire::integer(&event.payload, "timestamp"),
            ) else {
                continue;
            };
            let mut quotes = shared.quotes.write().await;
            let book = quotes.entry(symbol).or_default();
            let received = Instant::now();
            if let Ok(bid) = wire::number(&event.payload, "bid")
                && book.bid.is_none_or(|(_, _, previous)| time >= previous)
            {
                book.bid = Some((bid / 100000.0, received, time));
            }
            if let Ok(ask) = wire::number(&event.payload, "ask")
                && book.ask.is_none_or(|(_, _, previous)| time >= previous)
            {
                book.ask = Some((ask / 100000.0, received, time));
            }
            drop(quotes);
            shared.changed.notify_waiters();
        }
    });
    slots.push(Slot {
        config,
        session: shared.clone(),
    });
    Ok(shared)
}
impl Shared {
    pub(crate) fn risk_revision(&self) -> u64 {
        self.risk_revision.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub(crate) fn risk_healthy(&self) -> bool {
        self.risk_healthy.load(std::sync::atomic::Ordering::SeqCst) && !self.client.is_closed()
    }
    pub(crate) async fn risk_quote(&self, symbol: i64, age: i64) -> Result<(Quote, DateTime<Utc>)> {
        self.quote(symbol).await?;
        let books = self.quotes.read().await;
        let book = books
            .get(&symbol)
            .ok_or_else(|| eyre!("risk quote missing"))?;
        let (bid, ask) = book
            .bid
            .zip(book.ask)
            .ok_or_else(|| eyre!("paired risk quote missing"))?;
        let timestamp = DateTime::from_timestamp_millis(bid.2.min(ask.2))
            .ok_or_else(|| eyre!("invalid risk quote timestamp"))?;
        ensure!(
            crate::protection::quote_fresh(Utc::now(), timestamp, age)
                && bid.1.elapsed().as_secs_f64() <= age as f64
                && ask.1.elapsed().as_secs_f64() <= age as f64,
            "risk quote stale"
        );
        Ok((
            Quote {
                bid: bid.0,
                ask: ask.0,
            },
            timestamp,
        ))
    }

    pub async fn candles(&self) -> Result<Arc<candle_cache::ctrader::CachedFeed>> {
        Ok(self
            .candles
            .get_or_try_init(|| async {
                let source = ctrader_data_source::CtraderDataSource::from_client(
                    self.config.clone(),
                    self.client.clone(),
                )?;
                Ok::<_, color_eyre::Report>(Arc::new(candle_cache::ctrader::CachedFeed::new(
                    source,
                )))
            })
            .await?
            .clone())
    }
    pub async fn quote(&self, symbol: i64) -> Result<Quote> {
        let mut subscribed = self.subscriptions.lock().await;
        if !subscribed.contains(&symbol) {
            self.client.request(2127,2128,json!({"ctidTraderAccountId":self.client.account_id(),"symbolId":[symbol],"subscribeToSpotTimestamp":true})).await?;
            subscribed.insert(symbol);
        }
        drop(subscribed);
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Some(book) = self.quotes.read().await.get(&symbol)
                    && let Some((bid, ask)) = book.bid.zip(book.ask)
                    && fresh(bid.1, bid.2)
                    && fresh(ask.1, ask.2)
                {
                    ensure!(
                        bid.0 > 0.0 && ask.0 >= bid.0,
                        "invalid crossed cTrader quote"
                    );
                    return Ok(Quote {
                        bid: bid.0,
                        ask: ask.0,
                    });
                }
                ensure!(
                    !self.client.is_closed(),
                    "cTrader quote session disconnected"
                );
                changed.await;
            }
        })
        .await
        .map_err(|_| eyre!("fresh paired cTrader quote unavailable within 8 seconds"))?
    }
}
fn fresh(received: Instant, timestamp: i64) -> bool {
    let age = Utc::now().timestamp_millis() - timestamp;
    received.elapsed() < Duration::from_secs(30) && (-2000..=30000).contains(&age)
}
