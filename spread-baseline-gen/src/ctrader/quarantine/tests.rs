use super::*;
use audit::Audit;
use candle_model::BidAskCandleData;
use chrono::TimeZone;
use color_eyre::eyre::eyre;
use std::{cell::RefCell, path::Path};

fn time(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap()
}
fn binding() -> Binding {
    crate::ctrader::test_logging();
    Binding {
        version: 1,
        identity: Identity {
            profile: "demo".into(),
            environment: "demo".into(),
            endpoint: ctrader_api::Environment::Demo.endpoint().into(),
            account_id: 42,
            broker: "Example".into(),
            login: 123,
        },
        from_utc: time(0),
        to_utc: time(90 * 86_400_000),
        requested_symbols: vec!["ETHEUR".into()],
        policy: Policy::new(5).unwrap(),
    }
}
fn minute(ms: i64) -> Minute {
    Minute::from_error(
        "ETHEUR",
        7,
        &ctrader_data_source::CrossedHistoricalQuote {
            time_ms: ms,
            bid: 2.0,
            ask: 1.0,
        },
    )
    .unwrap()
}
fn audit(path: &Path) -> Audit {
    Audit::open(path, None, binding()).unwrap()
}
fn bar(ms: i64) -> BidAskCandleData {
    BidAskCandleData {
        timestamp: time(ms).fixed_offset(),
        open: 1.5,
        high: 1.5,
        low: 1.5,
        close: 1.5,
        volume: 1.0,
        bid_open: 1.0,
        bid_high: 1.0,
        bid_low: 1.0,
        bid_close: 1.0,
        ask_open: 2.0,
        ask_high: 2.0,
        ask_low: 2.0,
        ask_close: 2.0,
    }
}

#[test]
fn durable_audit_deduplicates_minutes_and_binds_resume_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.json");
    let mut a = audit(&path);
    a.record(minute(61_000)).unwrap();
    a.record(minute(62_000)).unwrap();
    assert_eq!(a.minutes("ETHEUR", 7).unwrap().len(), 1);
    let evidence = a.evidence().unwrap();
    assert_eq!(evidence.excluded_minutes, 1);
    drop(a);
    let a = audit(&path);
    assert_eq!(a.minutes("ETHEUR", 7).unwrap().len(), 1);
    drop(a);
    let mut other = binding();
    other.identity.account_id += 1;
    assert!(Audit::open(&path, None, other).is_err());
    let mut other = binding();
    other.policy = Policy::new(1).unwrap();
    assert!(Audit::open(&path, None, other).is_err());
}
#[test]
fn audit_budget_and_native_id_cannot_be_reset_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.json");
    let mut b = binding();
    b.policy = Policy::new(1).unwrap();
    let mut a = Audit::open(&path, None, b.clone()).unwrap();
    a.record(minute(61_000)).unwrap();
    drop(a);
    let mut a = Audit::open(&path, None, b).unwrap();
    assert!(a.record(minute(121_000)).is_err());
    assert!(a.minutes("ETHEUR", 8).is_err());
    assert_eq!(a.minutes("ETHEUR", 7).unwrap().len(), 1);
}
#[test]
fn failed_audit_write_does_not_accept_an_exclusion() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.json");
    let mut a = audit(&path);
    a.path = dir.path().into();
    assert!(a.record(minute(61_000)).is_err());
    assert!(a.minutes("ETHEUR", 7).unwrap().is_empty());
}
#[tokio::test]
async fn crossed_minute_is_wholly_omitted_and_valid_segments_are_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = audit(&dir.path().join("audit.json"));
    let calls = RefCell::new(Vec::new());
    let rows = collect::collect(time(0), time(180_000), "ETHEUR", 7, &mut a, |from, to| {
        calls.borrow_mut().push((from, to));
        async move {
            if from <= time(61_000) && time(61_000) < to {
                return Err(eyre!(ctrader_data_source::CrossedHistoricalQuote {
                    time_ms: 61_000,
                    bid: 2.0,
                    ask: 1.0
                }));
            }
            Ok((from.timestamp_millis()..to.timestamp_millis())
                .step_by(60_000)
                .map(bar)
                .collect())
        }
    })
    .await
    .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r.timestamp.timestamp_millis())
            .collect::<Vec<_>>(),
        [0, 120_000]
    );
    assert_eq!(
        *calls.borrow(),
        [
            (time(0), time(180_000)),
            (time(0), time(60_000)),
            (time(120_000), time(180_000))
        ]
    );
    assert_eq!(a.minutes("ETHEUR", 7).unwrap(), [minute(61_000)]);
}
#[tokio::test]
async fn warm_resume_skips_known_bad_minutes_but_retains_the_audit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.json");
    let mut a = audit(&path);
    a.record(minute(61_000)).unwrap();
    drop(a);
    let mut a = audit(&path);
    let calls = RefCell::new(Vec::new());
    let rows = collect::collect(time(0), time(180_000), "ETHEUR", 7, &mut a, |from, to| {
        calls.borrow_mut().push((from, to));
        async move {
            ensure!(
                to <= time(60_000) || from >= time(120_000),
                "bad minute fetched"
            );
            Ok(vec![bar(from.timestamp_millis())])
        }
    })
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(calls.borrow().len(), 2);
    assert_eq!(a.evidence().unwrap().excluded_minutes, 1);
}
#[tokio::test]
async fn unrelated_errors_and_invalid_cross_metadata_still_fail() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = audit(&dir.path().join("audit.json"));
    assert!(
        collect::collect(time(0), time(180_000), "ETHEUR", 7, &mut a, |_, _| async {
            Err(eyre!("native BID mismatch"))
        })
        .await
        .is_err()
    );
    assert!(a.minutes("ETHEUR", 7).unwrap().is_empty());
    for (stamp, bid, ask) in [
        (180_000, 2.0, 1.0),
        (61_000, f64::NAN, 1.0),
        (61_000, 1.0, 2.0),
    ] {
        assert!(
            collect::collect(
                time(0),
                time(180_000),
                "ETHEUR",
                7,
                &mut a,
                |_, _| async move {
                    Err(eyre!(ctrader_data_source::CrossedHistoricalQuote {
                        time_ms: stamp,
                        bid,
                        ask
                    }))
                }
            )
            .await
            .is_err()
        );
    }
    assert!(a.minutes("ETHEUR", 7).unwrap().is_empty());
}

#[tokio::test]
async fn crossed_book_carried_over_a_boundary_consumes_distinct_minutes() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = audit(&dir.path().join("audit.json"));
    let rows = collect::collect(
        time(0),
        time(240_000),
        "ETHEUR",
        7,
        &mut a,
        |from, to| async move {
            let stamp = if from <= time(61_000) && time(61_000) < to {
                Some(61_000)
            } else if from <= time(120_000) && time(120_000) < to {
                Some(120_000)
            } else {
                None
            };
            if let Some(time_ms) = stamp {
                return Err(eyre!(ctrader_data_source::CrossedHistoricalQuote {
                    time_ms,
                    bid: 2.0,
                    ask: 1.0
                }));
            }
            Ok((from.timestamp_millis()..to.timestamp_millis())
                .step_by(60_000)
                .map(bar)
                .collect())
        },
    )
    .await
    .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r.timestamp.timestamp_millis())
            .collect::<Vec<_>>(),
        [0, 180_000]
    );
    assert_eq!(a.minutes("ETHEUR", 7).unwrap().len(), 2);
}

#[tokio::test]
async fn audit_failure_prevents_followup_fetches_and_budget_overrun_remains_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.json");
    let mut a = audit(&path);
    a.path = dir.path().into();
    let calls = RefCell::new(0);
    assert!(
        collect::collect(time(0), time(180_000), "ETHEUR", 7, &mut a, |_, _| {
            *calls.borrow_mut() += 1;
            async {
                Err(eyre!(ctrader_data_source::CrossedHistoricalQuote {
                    time_ms: 61_000,
                    bid: 2.0,
                    ask: 1.0
                }))
            }
        })
        .await
        .is_err()
    );
    assert_eq!(*calls.borrow(), 1);
    assert!(a.minutes("ETHEUR", 7).unwrap().is_empty());
    drop(a);
    let mut b = binding();
    b.policy = Policy::new(1).unwrap();
    let mut a = Audit::open(&dir.path().join("one.json"), None, b).unwrap();
    let error = collect::collect(
        time(0),
        time(180_000),
        "ETHEUR",
        7,
        &mut a,
        |from, _| async move {
            Err(eyre!(ctrader_data_source::CrossedHistoricalQuote {
                time_ms: from.timestamp_millis(),
                bid: 2.0,
                ask: 1.0
            }))
        },
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("budget exhausted"));
    assert_eq!(a.evidence().unwrap().excluded_minutes, 1);
}

#[tokio::test]
async fn source_failure_after_an_exclusion_is_not_converted_to_success() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = audit(&dir.path().join("audit.json"));
    let error = collect::collect(
        time(0),
        time(180_000),
        "ETHEUR",
        7,
        &mut a,
        |from, to| async move {
            if from <= time(61_000) && time(61_000) < to {
                return Err(eyre!(ctrader_data_source::CrossedHistoricalQuote {
                    time_ms: 61_000,
                    bid: 2.0,
                    ask: 1.0
                }));
            }
            Err(eyre!("missing historical seed"))
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("missing historical seed"));
    assert_eq!(a.evidence().unwrap().excluded_minutes, 1);
}

#[test]
fn out_of_window_and_duplicate_on_disk_records_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.json");
    let mut a = audit(&path);
    assert!(a.record(minute(-1)).is_err());
    assert!(a.record(minute(90 * 86_400_000)).is_err());
    a.record(minute(61_000)).unwrap();
    drop(a);
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let records = value["records"].as_array_mut().unwrap();
    records.push(records[0].clone());
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(Audit::open(&path, None, binding()).is_err());
}

#[test]
fn resume_into_another_report_carries_the_durable_budget_and_original_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("old.json");
    let mut a = audit(&original);
    a.record(minute(61_000)).unwrap();
    let before = a.evidence().unwrap();
    drop(a);
    let copied = dir.path().join("new.json");
    let a = Audit::open(&copied, Some(&original), binding()).unwrap();
    assert_eq!(a.minutes("ETHEUR", 7).unwrap(), [minute(61_000)]);
    assert_eq!(a.evidence().unwrap().sha256, before.sha256);
    assert_eq!(a.evidence().unwrap().path, copied);
    assert_eq!(audit::read(&original).unwrap().1, before.sha256);
}

#[tokio::test]
async fn valid_segment_fetches_observe_the_already_persisted_exclusion() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.json");
    let mut a = audit(&path);
    collect::collect(time(0), time(180_000), "ETHEUR", 7, &mut a, |from, to| {
        let path = &path;
        async move {
            if from <= time(61_000) && time(61_000) < to {
                return Err(eyre!(ctrader_data_source::CrossedHistoricalQuote {
                    time_ms: 61_000,
                    bid: 2.0,
                    ask: 1.0
                }));
            }
            assert_eq!(audit::read(path).unwrap().0.records, [minute(61_000)]);
            Ok(vec![bar(from.timestamp_millis())])
        }
    })
    .await
    .unwrap();
}

#[test]
fn bare_relative_audit_filename_uses_the_current_directory() {
    let file = tempfile::Builder::new()
        .prefix(".quarantine-relative-")
        .suffix(".json")
        .tempfile_in(".")
        .unwrap();
    let path = std::path::PathBuf::from(file.path().file_name().unwrap());
    file.close().unwrap();
    let mut a = Audit::open(&path, None, binding()).unwrap();
    a.record(minute(61_000)).unwrap();
    assert!(a.evidence().unwrap().path.is_absolute());
    drop(a);
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(path.with_extension("json.lock")).unwrap();
}

#[test]
fn existing_resume_target_cannot_erase_or_replace_source_exclusions() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original.json");
    let stale = dir.path().join("stale.json");
    let mut source = audit(&original);
    source.record(minute(61_000)).unwrap();
    drop(source);
    drop(audit(&stale));
    assert!(Audit::open(&stale, Some(&original), binding()).is_err());
    let conflict = dir.path().join("conflict.json");
    let mut target = audit(&conflict);
    let mut other = minute(61_000);
    other.bid = 3.0;
    target.record(other).unwrap();
    drop(target);
    assert!(Audit::open(&conflict, Some(&original), binding()).is_err());
    let newer = dir.path().join("newer.json");
    let mut target = audit(&newer);
    target.record(minute(61_000)).unwrap();
    target.record(minute(121_000)).unwrap();
    drop(target);
    let target = Audit::open(&newer, Some(&original), binding()).unwrap();
    assert_eq!(
        target.minutes("ETHEUR", 7).unwrap(),
        [minute(61_000), minute(121_000)]
    );
}
