//! Single-flight authentication and freshness checks per account.

use std::{collections::BTreeMap, future::Future, sync::Arc, time::Duration};

use tokio::{
    sync::{Mutex, RwLock},
    task::JoinSet,
    time::{Instant, timeout},
};

use super::schedule::{ACTIVE_CHECK, Schedule};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(45);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct Key {
    pub name: String,
    pub live: bool,
}

pub(super) enum Probe {
    Healthy,
    Expired,
    Inconclusive,
}

pub(super) trait Backend: Send + Sync + 'static {
    type Session: Send + Sync;
    fn login(&self, key: &Key) -> impl Future<Output = Result<Self::Session, String>> + Send;
    fn probe(&self, session: &Self::Session) -> impl Future<Output = Probe> + Send;
}

struct Cached<S> {
    session: Option<Arc<S>>,
    last_used: Option<Instant>,
    checked_at: Option<Instant>,
}

struct Entry<S> {
    cached: RwLock<Cached<S>>,
    schedule: Arc<Mutex<Schedule>>,
    created_at: Instant,
}

impl<S> Cached<S> {
    fn fresh(&self) -> Option<Arc<S>> {
        self.session.clone().filter(|_| {
            self.checked_at
                .is_some_and(|checked| checked.elapsed() < ACTIVE_CHECK)
        })
    }
}

pub(super) struct Pool<B: Backend> {
    pub(super) backend: B,
    entries: RwLock<BTreeMap<Key, Arc<Entry<B::Session>>>>,
}

impl<B: Backend> Pool<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            entries: RwLock::new(BTreeMap::new()),
        }
    }

    async fn entry(&self, key: &Key) -> Arc<Entry<B::Session>> {
        if let Some(entry) = self.entries.read().await.get(key) {
            return entry.clone();
        }
        self.entries
            .write()
            .await
            .entry(key.clone())
            .or_insert_with(|| {
                Arc::new(Entry {
                    cached: RwLock::new(Cached {
                        session: None,
                        last_used: None,
                        checked_at: None,
                    }),
                    schedule: Arc::new(Mutex::new(Schedule::new(key))),
                    created_at: Instant::now(),
                })
            })
            .clone()
    }

    pub async fn register(&self, key: &Key) {
        self.entry(key).await;
    }

    /// Removed or reclassified accounts must stop generating background logins.
    pub async fn reconcile(&self, keys: &[Key], discovery_started: Instant) {
        self.entries.write().await.retain(|key, entry| {
            keys.contains(key)
                || entry.created_at >= discovery_started
                || Arc::strong_count(entry) > 1
        });
        for key in keys {
            self.register(key).await;
        }
    }

    pub async fn get(&self, key: &Key) -> Result<Arc<B::Session>, String> {
        let entry = self.entry(key).await;
        let existing = {
            let mut cached = entry.cached.write().await;
            cached.last_used = Some(Instant::now());
            cached.fresh()
        };
        if let Some(session) = existing {
            if let Ok(mut schedule) = entry.schedule.try_lock() {
                schedule.used();
            }
            return Ok(session);
        }
        let mut schedule = entry.schedule.lock().await;
        if let Some(session) = entry.cached.read().await.fresh() {
            return Ok(session);
        }
        if let Some(error) = schedule.cooldown() {
            // An inconclusive probe never proves expiry. Retain its session
            // during backoff; confirmed expiry already cleared the cache.
            return entry.cached.read().await.session.clone().ok_or(error);
        }
        self.refresh(key, &entry, &mut schedule).await
    }

    async fn login(
        &self,
        key: &Key,
        entry: &Entry<B::Session>,
        schedule: &mut Schedule,
    ) -> Result<Arc<B::Session>, String> {
        let result = timeout(LOGIN_TIMEOUT, self.backend.login(key))
            .await
            .unwrap_or_else(|_| Err("TradeNation login timed out after 45s".into()));
        match result {
            Ok(session) => {
                let session = Arc::new(session);
                let mut cached = entry.cached.write().await;
                cached.session = Some(session.clone());
                cached.checked_at = Some(Instant::now());
                schedule.healthy(cached.last_used);
                tracing::info!(
                    account = key.name,
                    live = key.live,
                    "TradeNation pooled session ready"
                );
                Ok(session)
            }
            Err(error) => {
                schedule.failed(error.clone());
                tracing::warn!(account = key.name, live = key.live, %error, "TradeNation session renewal failed; backing off");
                Err(error)
            }
        }
    }

    /// Schedule independently so one slow account cannot hold up another.
    /// The owned guard prevents overlapping foreground/background renewals.
    pub async fn maintain_due(self: &Arc<Self>, jobs: &mut JoinSet<()>) {
        let entries: Vec<_> = self
            .entries
            .read()
            .await
            .iter()
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect();
        for (key, entry) in entries {
            let Ok(mut schedule) = entry.schedule.clone().try_lock_owned() else {
                continue;
            };
            if schedule.next > Instant::now() {
                continue;
            }
            let pool = self.clone();
            jobs.spawn(async move {
                pool.refresh(&key, &entry, &mut schedule).await.ok();
            });
        }
    }

    async fn refresh(
        &self,
        key: &Key,
        entry: &Entry<B::Session>,
        schedule: &mut Schedule,
    ) -> Result<Arc<B::Session>, String> {
        let session = entry.cached.read().await.session.clone();
        if let Some(session) = session {
            let probe = timeout(PROBE_TIMEOUT, self.backend.probe(&session))
                .await
                .unwrap_or(Probe::Inconclusive);
            match probe {
                Probe::Healthy => {
                    let mut cached = entry.cached.write().await;
                    cached.checked_at = Some(Instant::now());
                    schedule.healthy(cached.last_used);
                    return Ok(session);
                }
                Probe::Inconclusive => {
                    schedule.failed("session health probe inconclusive".into());
                    tracing::warn!(
                        account = key.name,
                        "TradeNation session probe inconclusive; retaining session and backing off"
                    );
                    return Ok(session);
                }
                Probe::Expired => {
                    entry.cached.write().await.session = None;
                    tracing::warn!(
                        account = key.name,
                        "TradeNation pooled session expired; renewing"
                    );
                }
            }
        }
        self.login(key, entry, schedule).await
    }
}
