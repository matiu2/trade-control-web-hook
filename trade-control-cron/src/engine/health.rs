//! Failures before evaluation still belong in the plan's journal.
use chrono::{DateTime, Utc};
use trade_control_core::{
    recording::{CronNote, CronNoteSeverity},
    state::StoredPlan,
};

pub(crate) fn observation(
    stored: &StoredPlan,
    error: Option<&str>,
    now: DateTime<Utc>,
) -> CronNote {
    CronNote {
        ts: now.to_rfc3339(),
        trade_id: stored.plan.trade_id.clone(),
        account: stored.account.clone(),
        source: "engine-health".into(),
        severity: if error.is_some() {
            CronNoteSeverity::Error
        } else {
            CronNoteSeverity::Info
        },
        message: match error {
            Some(error) => format!(
                "{} {:?}: {error}; plan evaluation failed (entries and protection rules unavailable this tick)",
                stored.plan.instrument, stored.plan.granularity
            ),
            None => format!(
                "{} {:?}: plan evaluation resumed; this does not confirm an entry; closed-candle catch-up is reported separately",
                stored.plan.instrument, stored.plan.granularity
            ),
        },
        broker_exit: None,
        attempt_resolution: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BrokerHandle, CronEnv, run_engine_tick};
    use std::cell::RefCell;
    use tracing_subscriber::prelude::*;
    use trade_control_core::{
        dispatch_config::DispatchConfig,
        incoming::Verified,
        state::{MemStateStore, StateStore},
        tick_bundle::TickBundle,
    };

    struct Offline(RefCell<Vec<CronNote>>);
    impl CronEnv for Offline {
        async fn acquire_broker(&self, _: Option<&str>) -> Option<BrokerHandle> {
            None
        }
        async fn acquire_engine_broker(&self, _: Option<&str>) -> Result<BrokerHandle, String> {
            Err("MT5 account/server mismatch".into())
        }
        async fn dispatch_config(&self, _: &Verified) -> DispatchConfig {
            unreachable!()
        }
        fn record_tick(&self, _: TickBundle) {
            panic!("no evaluation occurred")
        }
        fn record_cron_note(&self, note: CronNote) {
            self.0.borrow_mut().push(note);
        }
        fn signing_key(&self) -> Option<Vec<u8>> {
            None
        }
    }

    #[test]
    fn unavailable_broker_is_journalled_before_first_seed_without_consuming_a_signal() {
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer())
            .try_init()
            .ok();
        pollster::block_on(async {
            let store = MemStateStore::default();
            let plan = serde_json::from_value(
                serde_json::json!({"trade_id":"gbpaud", "instrument":"GBPAUD",
                "direction":"short", "granularity":"m15", "pip_size":0.0001, "rules":[]}),
            )
            .unwrap();
            store
                .put_trade_plan(Some("competition"), &plan)
                .await
                .unwrap();
            let cron = Offline(RefCell::new(vec![]));
            run_engine_tick(&store, &cron, Utc::now()).await;
            let notes = cron.0.borrow().clone();
            assert_eq!(notes.len(), 1);
            assert_eq!(notes[0].account.as_deref(), Some("competition"));
            assert_eq!(notes[0].trade_id, "gbpaud");
            assert_eq!(notes[0].severity, CronNoteSeverity::Error);
            assert!(notes[0].message.contains("MT5 account/server mismatch"));
            assert!(
                store
                    .get_plan_state(Some("competition"), "gbpaud")
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(store.list_all_trade_plans().await.unwrap().len(), 1);
        });
    }
}
