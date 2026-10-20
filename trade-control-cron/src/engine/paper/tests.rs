use super::*;
use std::cell::{Cell, RefCell};
use tracing_subscriber::prelude::*;
use trade_control_core::{
    broker::Candle, dispatch_config::DispatchConfig, incoming::Verified, tick_bundle::TickBundle,
};

struct Recorder {
    notes: RefCell<Vec<(String, CronNote)>>,
    fail: bool,
}

impl CronEnv for Recorder {
    async fn acquire_broker(&self, _: Option<&str>) -> Option<crate::BrokerHandle> {
        panic!("paper logging must not contact the broker")
    }
    async fn dispatch_config(&self, _: &Verified) -> DispatchConfig {
        panic!("paper logging uses the signed plan's price units")
    }
    fn record_tick(&self, _: TickBundle) {}
    fn record_cron_note(&self, _: CronNote) {
        panic!("paper entries need the durable recording method")
    }
    async fn record_paper_entry(&self, identity: &str, note: CronNote) -> Result<(), String> {
        if self.fail {
            return Err("disk unavailable".into());
        }
        self.notes.borrow_mut().push((identity.into(), note));
        Ok(())
    }
    fn signing_key(&self) -> Option<Vec<u8>> {
        None
    }
}

fn init() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .ok();
}

fn stored() -> StoredPlan {
    StoredPlan {
        account: Some("healthy-data".into()),
        plan: serde_json::from_value(serde_json::json!({
            "trade_id":"paper-1","instrument":"AUDUSD","direction":"long",
            "granularity":"h1","pip_size":0.0001,"shadow":true,"rules":[]
        }))
        .unwrap(),
    }
}

fn entry() -> FiredIntent {
    FiredIntent {
        rule_id: "05-enter".into(),
        intent: serde_json::from_value(serde_json::json!({
            "v":1,"id":"entry-1","action":"enter","instrument":"AUDUSD","direction":"long",
            "not_after":"2030-01-01T00:00:00Z","risk_pct":1.0,
            "entry":{"type":"market"},"stop_loss":{"absolute":0.697},"take_profit":{"absolute":0.702}
        })).unwrap(),
        candle: Candle { time: "2026-10-20T01:00:00Z".parse().unwrap(), o:0.699,h:0.700,l:0.698,c:0.699 },
        signal: None,
    }
}

#[test]
fn paper_logs_resolved_brackets_before_advancing() {
    init();
    pollster::block_on(async {
        let cron = Recorder {
            notes: RefCell::new(vec![]),
            fail: false,
        };
        record_then_advance(&cron, &stored(), &[entry()], Utc::now(), || async {
            assert_eq!(
                cron.notes.borrow().len(),
                1,
                "note must commit before watermark"
            );
        })
        .await
        .unwrap();
        let notes = cron.notes.borrow();
        assert_eq!(notes[0].1.source, "paper-entry");
        assert!(notes[0].1.message.contains("market reference=0.699"));
        assert!(notes[0].1.message.contains("SL=0.697 TP=0.702"));
        assert!(notes[0].1.message.contains("no broker order, fill or P&L"));
    });
}

#[test]
fn failed_recording_preserves_watermark_and_retries_keep_identity() {
    init();
    pollster::block_on(async {
        let cron = Recorder {
            notes: RefCell::new(vec![]),
            fail: true,
        };
        let advanced = Cell::new(false);
        let error = record_then_advance(&cron, &stored(), &[entry()], Utc::now(), || async {
            advanced.set(true);
        })
        .await
        .unwrap_err();
        assert!(error.contains("disk unavailable"));
        assert!(!advanced.get());
        let mut later = entry();
        assert_eq!(entry_identity(&entry()), entry_identity(&later));
        later.candle.time += chrono::Duration::hours(1);
        assert_ne!(entry_identity(&entry()), entry_identity(&later));
    });
}

#[test]
fn live_fires_and_non_entry_rules_do_not_write_paper_notes() {
    init();
    pollster::block_on(async {
        let cron = Recorder {
            notes: RefCell::new(vec![]),
            fail: false,
        };
        let mut live = stored();
        live.plan.shadow = false;
        record_then_advance(&cron, &live, &[entry()], Utc::now(), || async {})
            .await
            .unwrap();
        let mut close = entry();
        close.intent.action = Action::Close;
        record_then_advance(&cron, &stored(), &[close], Utc::now(), || async {})
            .await
            .unwrap();
        assert!(cron.notes.borrow().is_empty());
    });
}

#[test]
fn unsupported_runtime_cannot_consume_a_paper_signal() {
    struct Unsupported;
    impl CronEnv for Unsupported {
        async fn acquire_broker(&self, _: Option<&str>) -> Option<crate::BrokerHandle> {
            unreachable!()
        }
        async fn dispatch_config(&self, _: &Verified) -> DispatchConfig {
            unreachable!()
        }
        fn record_tick(&self, _: TickBundle) {
            unreachable!()
        }
        fn record_cron_note(&self, _: CronNote) {
            panic!("a fire-and-forget sink cannot guarantee persistence")
        }
        fn signing_key(&self) -> Option<Vec<u8>> {
            None
        }
    }
    pollster::block_on(async {
        let advanced = Cell::new(false);
        let error =
            record_then_advance(&Unsupported, &stored(), &[entry()], Utc::now(), || async {
                advanced.set(true);
            })
            .await
            .unwrap_err();
        assert!(error.contains("unavailable in this runtime"));
        assert!(!advanced.get());
    });
}

#[test]
fn invalid_geometry_is_an_explicit_paper_decline() {
    let mut invalid = entry();
    invalid.intent.direction = Some(trade_control_core::intent::Direction::Short);
    let note = entry_note(&stored(), &invalid, Utc::now());
    assert_eq!(note.severity, CronNoteSeverity::Warn);
    assert!(note.message.contains("entry declined at resolution"));
}

#[test]
fn latched_pattern_extremes_resolve_the_same_stop_bracket_as_replay() {
    use trade_control_core::intent::{Direction, SignalKind};
    use trade_control_core::signals::{LatchedSignal, SigState};
    let mut fired = entry();
    fired.intent.entry = Some(
        serde_json::from_value(serde_json::json!({"type":"stop", "from":"signal_high"})).unwrap(),
    );
    fired.intent.stop_loss =
        Some(serde_json::from_value(serde_json::json!({"from":"signal_low"})).unwrap());
    fired.intent.take_profit =
        Some(serde_json::from_value(serde_json::json!({"offset_r":2.0})).unwrap());
    fired.signal = Some(LatchedSignal {
        direction: Direction::Long,
        kind: SignalKind::Pinbar,
        signal_high: 0.7001,
        signal_low: 0.6975,
        signal_range: 0.0026,
        signal_start_time: fired.candle.time,
        signal_bar_time: fired.candle.time,
        golden: true,
        signal_confirmed: true,
        band_anchor: 0.6980,
        atr: Some(0.001),
        recent_high: Some(0.7005),
        recent_low: Some(0.6970),
        state: SigState::Valid,
        fires: true,
    });
    let note = entry_note(&stored(), &fired, Utc::now());
    assert_eq!(note.severity, CronNoteSeverity::Info, "{}", note.message);
    assert!(
        note.message.contains("stop trigger=0.7001"),
        "{}",
        note.message
    );
    assert!(note.message.contains("SL=0.6975"), "{}", note.message);
}
