//! Coordinate broker jobs with offline administrative plan transitions.
//!
//! Jobs run concurrently under a shared guard. An admin transition waits for
//! in-flight jobs, then exclusively changes the plan and its management rows.
//! This prevents a previously loaded cron snapshot from dispatching after a
//! successful paper/swap/manual-close response.

use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

static OPERATIONS: RwLock<()> = RwLock::const_new(());

pub(crate) struct Guard {
    _read: Option<RwLockReadGuard<'static, ()>>,
    _write: Option<RwLockWriteGuard<'static, ()>>,
}

pub(crate) async fn acquire(exclusive: bool) -> Guard {
    if exclusive {
        Guard {
            _read: None,
            _write: Some(OPERATIONS.write().await),
        }
    } else {
        Guard {
            _read: Some(OPERATIONS.read().await),
            _write: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn administrative_transition_waits_for_existing_jobs() {
        let job = acquire(false).await;
        assert!(OPERATIONS.try_write().is_err());
        drop(job);
        let admin = acquire(true).await;
        assert!(OPERATIONS.try_read().is_err());
        drop(admin);
        assert!(OPERATIONS.try_read().is_ok());
    }
}
