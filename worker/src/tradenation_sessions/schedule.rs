//! Health cadence and bounded outage backoff, measured after each completed attempt.

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    time::Duration,
};
use tokio::time::Instant;

use super::pool::Key;

pub(super) const ACTIVE_CHECK: Duration = Duration::from_secs(30);
const ACTIVE_WINDOW: Duration = Duration::from_secs(120);
const IDLE_CHECK: Duration = Duration::from_secs(300);

pub(super) struct Schedule {
    pub next: Instant,
    failures: u32,
    error: Option<String>,
    stagger: Duration,
}

impl Schedule {
    pub fn new(key: &Key) -> Self {
        let mut hash = DefaultHasher::new();
        key.hash(&mut hash);
        Self {
            next: Instant::now(),
            failures: 0,
            error: None,
            stagger: Duration::from_secs(hash.finish() % 5),
        }
    }

    pub fn used(&mut self) {
        if self.failures == 0 {
            self.next = self.next.min(Instant::now() + ACTIVE_CHECK + self.stagger);
        }
    }

    pub fn healthy(&mut self, last_used: Option<Instant>) {
        self.failures = 0;
        self.error = None;
        let active = last_used.is_some_and(|used| used.elapsed() < ACTIVE_WINDOW);
        let interval = if active { ACTIVE_CHECK } else { IDLE_CHECK };
        self.next = Instant::now() + interval + self.stagger;
    }

    pub fn failed(&mut self, error: String) {
        self.failures = self.failures.saturating_add(1);
        self.error = Some(error);
        let seconds = (5_u64 << self.failures.saturating_sub(1).min(5)).min(120);
        self.next = Instant::now() + Duration::from_secs(seconds) + self.stagger;
    }

    pub fn cooldown(&self) -> Option<String> {
        let remaining = self.next.saturating_duration_since(Instant::now());
        self.error
            .as_ref()
            .filter(|_| !remaining.is_zero())
            .map(|error| {
                format!(
                    "session renewal retry in {}s: {error}",
                    remaining.as_secs() + 1
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn retry_backoff_is_capped_and_success_resets_it() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
        let key = Key {
            name: "test".into(),
            live: false,
        };
        let mut schedule = Schedule::new(&key);
        for seconds in [5, 10, 20, 40, 80, 120, 120] {
            schedule.failed("offline".into());
            assert_eq!(
                schedule.next - Instant::now(),
                Duration::from_secs(seconds) + schedule.stagger
            );
            let next = schedule.next;
            schedule.used();
            assert_eq!(schedule.next, next, "usage must not bypass outage backoff");
        }
        schedule.healthy(Some(Instant::now()));
        schedule.failed("offline".into());
        assert_eq!(
            schedule.next - Instant::now(),
            Duration::from_secs(5) + schedule.stagger
        );
    }
}
