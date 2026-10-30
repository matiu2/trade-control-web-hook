use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use tokio::{
    sync::{Mutex, Notify},
    task::JoinSet,
};
use tracing_subscriber::prelude::*;

use super::pool::{Backend, Key, Pool, Probe};

#[derive(Default)]
struct Fake {
    logins: AtomicUsize,
    probes: AtomicUsize,
    login_results: Mutex<VecDeque<Result<usize, String>>>,
    probe_results: Mutex<VecDeque<Probe>>,
    block_probe: Notify,
    probe_started: Notify,
    block: std::sync::atomic::AtomicBool,
    block_login: std::sync::atomic::AtomicBool,
    login_started: Notify,
    release_login: Notify,
}

impl Backend for Fake {
    type Session = usize;

    async fn login(&self, _: &Key) -> Result<usize, String> {
        let count = self.logins.fetch_add(1, Ordering::SeqCst) + 1;
        tokio::task::yield_now().await;
        if self.block_login.load(Ordering::SeqCst) {
            self.login_started.notify_one();
            self.release_login.notified().await;
        }
        self.login_results
            .lock()
            .await
            .pop_front()
            .unwrap_or(Ok(count))
    }

    async fn probe(&self, _: &usize) -> Probe {
        self.probes.fetch_add(1, Ordering::SeqCst);
        if self.block.load(Ordering::SeqCst) {
            self.probe_started.notify_one();
            self.block_probe.notified().await;
        }
        self.probe_results
            .lock()
            .await
            .pop_front()
            .unwrap_or(Probe::Healthy)
    }
}

fn pool() -> Arc<Pool<Fake>> {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    Arc::new(Pool::new(Fake::default()))
}

fn key(name: &str, live: bool) -> Key {
    Key {
        name: name.into(),
        live,
    }
}

async fn maintenance(pool: &Arc<Pool<Fake>>, seconds: u64) {
    tokio::time::advance(Duration::from_secs(seconds)).await;
    let mut jobs = JoinSet::new();
    pool.maintain_due(&mut jobs).await;
    while let Some(result) = jobs.join_next().await {
        result.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn concurrent_acquisitions_share_one_login_and_isolate_accounts_and_kinds() {
    let pool = pool();
    let account = key("live", true);
    let (first, second) = tokio::join!(pool.get(&account), pool.get(&account));
    assert!(Arc::ptr_eq(&first.unwrap(), &second.unwrap()));
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 1);
    pool.get(&key("other", true)).await.unwrap();
    pool.get(&key("live", false)).await.unwrap();
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 3);
}

#[tokio::test(start_paused = true)]
async fn healthy_sessions_are_probed_without_periodic_reauthentication() {
    let pool = pool();
    let account = key("live", true);
    let original = pool.get(&account).await.unwrap();
    maintenance(&pool, 35).await;
    assert_eq!(pool.backend.probes.load(Ordering::SeqCst), 1);
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 1);
    assert!(Arc::ptr_eq(&original, &pool.get(&account).await.unwrap()));
}

#[tokio::test(start_paused = true)]
async fn transient_probe_failure_preserves_session_and_retries_without_login() {
    let pool = pool();
    let account = key("live", true);
    let original = pool.get(&account).await.unwrap();
    pool.backend
        .probe_results
        .lock()
        .await
        .push_back(Probe::Inconclusive);
    maintenance(&pool, 35).await;
    assert!(Arc::ptr_eq(&original, &pool.get(&account).await.unwrap()));
    maintenance(&pool, 10).await;
    assert_eq!(pool.backend.probes.load(Ordering::SeqCst), 2);
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn expiry_replaces_the_session_once() {
    let pool = pool();
    let account = key("live", true);
    let original = pool.get(&account).await.unwrap();
    pool.backend
        .probe_results
        .lock()
        .await
        .push_back(Probe::Expired);
    maintenance(&pool, 35).await;
    let (first, second) = tokio::join!(pool.get(&account), pool.get(&account));
    let replacement = first.unwrap();
    assert!(!Arc::ptr_eq(&original, &replacement));
    assert!(Arc::ptr_eq(&replacement, &second.unwrap()));
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn failed_renewal_never_returns_known_expired_session_or_starts_login_storm() {
    let pool = pool();
    let account = key("live", true);
    pool.get(&account).await.unwrap();
    pool.backend
        .probe_results
        .lock()
        .await
        .push_back(Probe::Expired);
    pool.backend
        .login_results
        .lock()
        .await
        .push_back(Err("502".into()));
    maintenance(&pool, 35).await;
    let (first, second) = tokio::join!(pool.get(&account), pool.get(&account));
    assert!(first.is_err());
    assert!(second.is_err());
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 2);
    maintenance(&pool, 10).await;
    assert!(pool.get(&account).await.is_ok());
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 3);
}

#[tokio::test(start_paused = true)]
async fn stale_acquisition_waits_for_probe_while_other_accounts_continue() {
    let pool = pool();
    let account = key("live", true);
    let original = pool.get(&account).await.unwrap();
    pool.backend.block.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(35)).await;
    let mut jobs = JoinSet::new();
    pool.maintain_due(&mut jobs).await;
    pool.backend.probe_started.notified().await;
    assert!(
        tokio::time::timeout(Duration::from_secs(1), pool.get(&account))
            .await
            .is_err()
    );
    assert!(pool.get(&key("other", true)).await.is_ok());
    pool.backend.block_probe.notify_one();
    jobs.join_next().await.unwrap().unwrap();
    assert!(Arc::ptr_eq(&original, &pool.get(&account).await.unwrap()));
}

#[tokio::test(start_paused = true)]
async fn idle_accounts_poll_less_often_and_usage_restores_active_cadence() {
    let pool = pool();
    let account = key("live", true);
    pool.get(&account).await.unwrap();
    maintenance(&pool, 180).await;
    assert_eq!(pool.backend.probes.load(Ordering::SeqCst), 1);
    maintenance(&pool, 35).await;
    assert_eq!(pool.backend.probes.load(Ordering::SeqCst), 1);
    pool.get(&account).await.unwrap();
    assert_eq!(pool.backend.probes.load(Ordering::SeqCst), 2);
    maintenance(&pool, 20).await;
    assert_eq!(pool.backend.probes.load(Ordering::SeqCst), 2);
    maintenance(&pool, 15).await;
    assert_eq!(pool.backend.probes.load(Ordering::SeqCst), 3);
}

#[tokio::test(start_paused = true)]
async fn discovery_prewarms_accounts_without_an_entry_request() {
    let pool = pool();
    let account = key("live", true);
    pool.register(&account).await;
    maintenance(&pool, 0).await;
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 1);
    assert_eq!(*pool.get(&account).await.unwrap(), 1);
}

#[tokio::test(start_paused = true)]
async fn failed_cold_login_is_shared_and_retried_only_after_backoff() {
    let pool = pool();
    let account = key("live", true);
    pool.backend
        .login_results
        .lock()
        .await
        .push_back(Err("502".into()));
    let (first, second) = tokio::join!(pool.get(&account), pool.get(&account));
    assert!(first.is_err());
    assert!(second.is_err());
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(pool.get(&account).await.is_ok());
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn a_timed_out_probe_keeps_the_existing_session() {
    let pool = pool();
    let account = key("live", true);
    let original = pool.get(&account).await.unwrap();
    pool.backend.block.store(true, Ordering::SeqCst);
    maintenance(&pool, 35).await;
    assert!(Arc::ptr_eq(&original, &pool.get(&account).await.unwrap()));
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn removed_accounts_are_no_longer_maintained() {
    let pool = pool();
    let account = key("live", true);
    pool.get(&account).await.unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    pool.reconcile(&[], tokio::time::Instant::now()).await;
    maintenance(&pool, 600).await;
    assert_eq!(pool.backend.probes.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn first_use_after_idle_renews_expired_session_before_returning() {
    let pool = pool();
    let account = key("live", true);
    let original = pool.get(&account).await.unwrap();
    tokio::time::advance(Duration::from_secs(180)).await;
    pool.backend
        .probe_results
        .lock()
        .await
        .push_back(Probe::Expired);
    let (first, second) = tokio::join!(pool.get(&account), pool.get(&account));
    let first = first.unwrap();
    assert!(!Arc::ptr_eq(&original, &first));
    assert!(Arc::ptr_eq(&first, &second.unwrap()));
    assert_eq!(pool.backend.probes.load(Ordering::SeqCst), 1);
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn old_discovery_snapshot_preserves_new_account_session() {
    let pool = pool();
    let started = tokio::time::Instant::now();
    tokio::time::advance(Duration::from_secs(1)).await;
    let account = key("new", true);
    let original = pool.get(&account).await.unwrap();
    pool.reconcile(&[], started).await;
    assert!(Arc::ptr_eq(&original, &pool.get(&account).await.unwrap()));
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn discovery_cannot_evict_an_inflight_login_and_break_single_flight() {
    let pool = pool();
    let account = key("new", true);
    pool.backend.block_login.store(true, Ordering::SeqCst);
    let first = tokio::spawn({
        let pool = pool.clone();
        let account = account.clone();
        async move { pool.get(&account).await }
    });
    pool.backend.login_started.notified().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    pool.reconcile(&[], tokio::time::Instant::now()).await;
    let second = tokio::spawn({
        let pool = pool.clone();
        let account = account.clone();
        async move { pool.get(&account).await }
    });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 1);
    pool.backend.release_login.notify_one();
    let first = first.await.unwrap().unwrap();
    assert!(Arc::ptr_eq(&first, &second.await.unwrap().unwrap()));
}

#[tokio::test(start_paused = true)]
async fn tokio_monitor_prewarms_polls_and_aborts_inflight_children_on_shutdown() {
    let pool = pool();
    let account = key("live", true);
    let monitor = tokio::spawn(super::maintenance::run(pool.clone(), {
        let account = account.clone();
        move || std::future::ready(Some(vec![account.clone()]))
    }));
    // Discovery and health work run in children. Explicit yields avoid the
    // paused clock automatically advancing past the deadline under test.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(5)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(pool.backend.logins.load(Ordering::SeqCst), 1);
    pool.get(&account).await.unwrap();
    pool.backend.block.store(true, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(35)).await;
    pool.backend.probe_started.notified().await;
    monitor.abort();
    assert!(monitor.await.unwrap_err().is_cancelled());
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    // Shutdown must release the per-account guard held by that probe.
    pool.backend.block.store(false, Ordering::SeqCst);
    maintenance(&pool, 0).await;
    assert_eq!(pool.backend.probes.load(Ordering::SeqCst), 2);
}
