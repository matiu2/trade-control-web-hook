use super::*;
use ctrader_fix::trading::Rejection;

struct Directory(PathBuf);
impl Directory {
    fn create() -> Result<Self> {
        super::super::tests::init();
        let path = std::env::temp_dir().join(Mutation::id());
        private_directory(&path)?;
        Ok(Self(path))
    }
    fn journal(&self) -> Result<Mutation> {
        Ok(Mutation {
            identity: json!({"sender_comp_id":"demo.broker.123"}),
            client_order_id: "cancel-1".into(),
            journal: Journal::prepare(
                &self.0.join("operation.jsonl"),
                &json!({"sender_comp_id":"demo.broker.123"}),
                &json!({"client_order_id":"cancel-1","operation":"cancel"}),
            )?,
        })
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}
#[test]
fn durable_intent_blocks_writes_until_positive_resolution() -> Result<()> {
    let directory = Directory::create()?;
    check_resolved(&directory.0)?;
    let mut journal = directory.journal()?;
    assert!(check_resolved(&directory.0).is_err());
    journal.record("outcome", &json!({"outcome":"unknown"}))?;
    drop(journal);
    assert!(
        check_resolved(&directory.0).is_err(),
        "new process cannot infer resolution from an unknown outcome"
    );
    Ok(())
}
#[test]
fn definitive_rejection_and_not_sent_can_resolve_without_broker_retry() -> Result<()> {
    let directory = Directory::create()?;
    let mut journal = directory.journal()?;
    journal.outcome(
        &Ok(MutationOutcome::Rejected(Rejection {
            client_order_id: "cancel-1".into(),
            code: None,
            reason: "rejected".into(),
        })),
        &[],
    )?;
    check_resolved(&directory.0)?;
    Ok(())
}
#[test]
fn verified_close_unlocks_following_mutations() -> Result<()> {
    let directory = Directory::create()?;
    let mut journal = directory.journal()?;
    journal.record("outcome", &json!({"outcome":"accepted"}))?;
    assert!(
        check_resolved(&directory.0).is_err(),
        "acceptance alone does not confirm full closure"
    );
    journal.resolve(&json!({"position_id":"7","flat":true}))?;
    check_resolved(&directory.0)?;
    Ok(())
}
#[test]
fn incomplete_and_insecure_evidence_blocks_writes() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let directory = Directory::create()?;
    let mut journal = directory.journal()?;
    journal.resolve(&json!({"submitted":false}))?;
    let path = directory.0.join("operation.jsonl");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
    assert!(check_resolved(&directory.0).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    use std::io::Write;
    OpenOptions::new()
        .append(true)
        .open(&path)?
        .write_all(b"{")?;
    assert!(check_resolved(&directory.0).is_err());
    Ok(())
}
#[test]
fn append_failure_preserves_structured_broker_receipt_before_returning_error() -> Result<()> {
    use std::io::Write;
    use tracing_subscriber::prelude::*;
    #[derive(Clone)]
    struct Output(Arc<StdMutex<Vec<u8>>>);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| std::io::Error::other("capture lock poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    super::super::tests::init();
    let output = Arc::new(StdMutex::new(Vec::new()));
    let captured = output.clone();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info"))
        .with(tracing_error::ErrorLayer::default())
        .with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_ansi(false)
                .with_writer(move || Output(captured.clone())),
        );
    let identity = json!({"sender_comp_id":"demo.broker.123","trade_host":"trade.example.com","trade_port":5202});
    let evidence = json!({"outcome":"accepted","order_id":"9","fill_price":1.101,"observed_events":[{"order_id":"10"}]});
    let receipt = receipt(&identity, "close-1", "outcome", &evidence);
    let result = tracing::subscriber::with_default(subscriber, || {
        append_with_receipt(&receipt, || {
            let bytes = output.lock().map_err(|_| eyre!("capture lock poisoned"))?;
            let logged = String::from_utf8_lossy(&bytes);
            assert!(
                logged.contains("close-1")
                    && logged.contains("demo.broker.123")
                    && logged.contains("1.101")
                    && logged.contains("observed_events")
            );
            Err::<(), _>(eyre!("injected disk failure"))
        })
    });
    assert!(result.is_err());
    assert_eq!(receipt["evidence"], evidence);
    assert_eq!(receipt["account_identity"], identity);
    Ok(())
}
#[test]
fn logout_append_attempts_every_journal_and_preserves_first_failure() {
    super::super::tests::init();
    let attempted = std::cell::Cell::new(0);
    let results = (0..3).map(|index| {
        attempted.set(attempted.get() + 1);
        match index {
            0 => Err(eyre!("first append failure")),
            1 => Err(eyre!("second append failure")),
            _ => Ok(()),
        }
    });
    let result = collect_append_results(results);
    assert_eq!(attempted.get(), 3);
    assert_eq!(result.unwrap_err().to_string(), "first append failure");
}
#[test]
fn directory_ancestry_is_persisted_before_any_intent_is_prepared() -> Result<()> {
    super::super::tests::init();
    let chain: Vec<_> = ["/", "/state", "/state/audit", "/state/audit/account"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let mut synced = Vec::new();
    let mut intent_prepared = false;
    let setup = (|| {
        persist_directory_links(&chain, |path| {
            synced.push(path.to_path_buf());
            if path == Path::new("/state") {
                return Err(eyre!("injected parent sync failure"));
            }
            Ok(())
        })?;
        intent_prepared = true;
        Ok::<_, color_eyre::Report>(())
    })();
    assert!(setup.is_err());
    assert!(!intent_prepared);
    assert_eq!(
        synced,
        chain.iter().rev().take(3).cloned().collect::<Vec<_>>()
    );
    synced.clear();
    persist_directory_links(&chain, |path| {
        synced.push(path.to_path_buf());
        Ok(())
    })?;
    assert_eq!(synced, chain.iter().rev().cloned().collect::<Vec<_>>());
    Ok(())
}
#[test]
fn append_failure_receipt_is_visible_when_info_logging_is_suppressed() -> Result<()> {
    use std::io::Write;
    use tracing_subscriber::prelude::*;
    #[derive(Clone)]
    struct Output(Arc<StdMutex<Vec<u8>>>);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| std::io::Error::other("capture lock poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    super::super::tests::init();
    let output = Arc::new(StdMutex::new(Vec::new()));
    let captured = output.clone();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("warn"))
        .with(tracing_error::ErrorLayer::default())
        .with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_ansi(false)
                .with_writer(move || Output(captured.clone())),
        );
    let identity = json!({"sender_comp_id":"demo.broker.123","trade_host":"trade.example.com","trade_port":5202});
    let evidence = json!({"order_id":"9","observed_events":[{"order_id":"10","fill_price":1.101}]});
    let receipt = receipt(&identity, "close-warn-1", "logout", &evidence);
    let result = tracing::subscriber::with_default(subscriber, || {
        append_with_receipt(&receipt, || {
            assert!(
                output
                    .lock()
                    .map_err(|_| eyre!("capture lock poisoned"))?
                    .is_empty(),
                "INFO receipt must be suppressed by WARN filter"
            );
            Err::<(), _>(eyre!("injected fsync failure"))
        })
    });
    assert!(result.is_err());
    let logged = String::from_utf8(
        output
            .lock()
            .map_err(|_| eyre!("capture lock poisoned"))?
            .clone(),
    )?;
    assert!(
        logged.contains("ERROR")
            && logged.contains("close-warn-1")
            && logged.contains("demo.broker.123")
            && logged.contains("logout")
            && logged.contains("1.101")
            && logged.contains("observed_events")
    );
    Ok(())
}
#[tokio::test]
async fn unresolved_fix_intent_blocks_companion_open_api_writes_without_environment_changes()
-> Result<()> {
    let directory = Directory::create()?;
    let (q, t) = super::super::tests::configs();
    let broker = FixBroker::connect("test-companion", q, t).await?;
    let lock = AccountLock::acquire_at(&broker, &directory.0)?;
    let _intent = Mutation::prepare(
        &broker,
        &lock,
        "unknown-fix-cancel".into(),
        json!({"operation":"cancel","order_id":"9"}),
    )?;
    drop(lock);
    assert!(
        broker
            .companion_write_at(
                "protected-entry",
                json!({"instrument":"EURUSD"}),
                Some(&directory.0)
            )
            .await
            .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn dropped_companion_and_unknown_outcomes_persistently_block_fix_writes() -> Result<()> {
    for record_unknown in [false, true] {
        let directory = Directory::create()?;
        let (q, t) = super::super::tests::configs();
        let broker = FixBroker::connect("test-companion", q, t).await?;
        let mut guard = broker
            .companion_write_at(
                "protected-entry",
                json!({"instrument":"EURUSD"}),
                Some(&directory.0),
            )
            .await?;
        assert!(!guard.id().is_empty());
        assert!(
            broker.gate.try_lock().is_err(),
            "companion write holds FIX account operation gate"
        );
        if record_unknown {
            guard.record(&json!({"outcome":"unknown"}), false)?;
        }
        drop(guard);
        assert!(
            AccountLock::acquire_at(&broker, &directory.0).is_err(),
            "FIX mutations must respect unresolved companion writes"
        );
    }
    Ok(())
}
#[tokio::test]
async fn positively_confirmed_companion_result_releases_shared_write_interlock() -> Result<()> {
    let directory = Directory::create()?;
    let (q, t) = super::super::tests::configs();
    let broker = FixBroker::connect("test-companion", q, t).await?;
    let mut guard = broker
        .companion_write_at(
            "protected-entry",
            json!({"instrument":"EURUSD"}),
            Some(&directory.0),
        )
        .await?;
    guard.record(&json!({"outcome":"accepted","order_id":"9"}), true)?;
    assert!(
        guard.record(&json!({"outcome":"unknown"}), false).is_err(),
        "outcomes cannot be rewritten after resolution"
    );
    drop(guard);
    let _lock = AccountLock::acquire_at(&broker, &directory.0)?;
    Ok(())
}
