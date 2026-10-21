use super::*;

fn init() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

fn row() -> Value {
    json!({"utcTimestampInMinutes":1000,"low":112250,"deltaOpen":2,"deltaHigh":9,"deltaClose":5,"volume":321})
}

fn response(rows: Value) -> Value {
    json!({"period":"M1","symbolId":123,"trendbar":rows})
}

#[test]
fn native_decoder_preserves_real_tick_volume_and_price_precision() {
    init();
    let bar = decode::bar(&row(), 5, NativePeriod::Minute1).expect("valid broker bar");
    assert_eq!(bar.time.timestamp(), 60000);
    assert_eq!(bar.volume, 321);
    assert_eq!(
        (bar.open, bar.high, bar.low, bar.close),
        (1.12252, 1.12259, 1.1225, 1.12255)
    );
    let rounded = decode::bar(&row(), 3, NativePeriod::Minute1).expect("rounded broker bar");
    assert_eq!(
        (rounded.open, rounded.high, rounded.low, rounded.close),
        (1.123, 1.123, 1.123, 1.123)
    );
}

#[test]
fn native_decoder_defaults_optional_zero_deltas_but_requires_price_time_volume() {
    init();
    let flat = json!({"utcTimestampInMinutes":"1000","low":"112250","volume":"0"});
    let bar = decode::bar(&flat, 5, NativePeriod::Minute1).expect("flat bar");
    assert_eq!(bar.open, bar.close);
    for field in ["utcTimestampInMinutes", "low", "volume"] {
        let mut missing = flat.clone();
        missing.as_object_mut().expect("object").remove(field);
        assert!(
            decode::bar(&missing, 5, NativePeriod::Minute1).is_err(),
            "missing {field}"
        );
    }
}

#[test]
fn native_decoder_rejects_invalid_geometry_overflow_negative_volume_and_wrong_period() {
    init();
    for (key, value) in [
        ("deltaOpen", json!(10)),
        ("deltaClose", json!(10)),
        ("deltaHigh", json!(-1)),
        ("low", json!(i64::MAX)),
        ("volume", json!(-1)),
        ("period", json!("H1")),
        ("utcTimestampInMinutes", json!(-1)),
    ] {
        let mut invalid = row();
        invalid[key] = value;
        assert!(
            decode::bar(&invalid, 5, NativePeriod::Minute1).is_err(),
            "invalid {key}"
        );
    }
    assert!(decode::bar(&row(), 9, NativePeriod::Minute1).is_err());
}

#[test]
fn page_validates_identity_period_shapes_and_half_open_range() {
    init();
    let valid = response(json!([row()]));
    assert!(decode::page(&valid, 123, NativePeriod::Minute1, 5, 60000000, 60060000).is_ok());
    assert!(decode::page(&valid, 123, NativePeriod::Minute1, 5, 60000001, 60060000).is_err());
    assert!(decode::page(&valid, 123, NativePeriod::Minute1, 5, 59940000, 60000000).is_err());
    for (key, value) in [
        ("period", json!(9)),
        ("symbolId", json!(999)),
        ("trendbar", json!({})),
        ("hasMore", json!("false")),
    ] {
        let mut invalid = valid.clone();
        invalid[key] = value;
        assert!(decode::page(&invalid, 123, NativePeriod::Minute1, 5, 60000000, 60060000).is_err());
    }
}

#[test]
fn request_windows_are_bounded_and_cover_exclusive_end_without_rounding() {
    init();
    let windows = ranges::windows(NativePeriod::Minute1, 1, 60000001).expect("windows");
    assert_eq!(windows.first().copied(), Some((1, 30000001)));
    assert_eq!(windows.last().copied(), Some((30000001, 60000001)));
    assert!(ranges::windows(NativePeriod::Minute1, -1, 10).is_err());
    assert!(ranges::windows(NativePeriod::Minute1, 1, 1).is_err());
    assert!(ranges::windows(NativePeriod::Minute1, 1, 2147483646001).is_err());
}

#[test]
fn backwards_pagination_requires_strict_progress_and_never_skips_boundary() {
    init();
    let bars = vec![decode::bar(&row(), 5, NativePeriod::Minute1).expect("bar")];
    assert_eq!(
        ranges::next_end(&bars, true, 59940000, 60120000).expect("progress"),
        Some(60000000)
    );
    assert_eq!(
        ranges::next_end(&bars, false, 59940000, 60120000).expect("complete"),
        None
    );
    assert!(ranges::next_end(&[], true, 59940000, 60120000).is_err());
    assert!(ranges::next_end(&bars, true, 60000000, 60120000).is_err());
}

#[test]
fn catalogue_is_sorted_serializable_and_rejects_duplicate_identity() {
    init();
    let catalogue = json!({"symbol":[{"symbolId":"2","symbolName":"EURUSD","description":"Euro vs Dollar","enabled":true},{"symbolId":1,"symbolName":"AUDUSD"}]});
    let symbols = catalogue::decode(&catalogue).expect("catalogue");
    assert_eq!(symbols[0].name, "AUDUSD");
    assert_eq!(symbols[0].enabled, None);
    assert_eq!(symbols[1].description.as_deref(), Some("Euro vs Dollar"));
    assert!(serde_json::to_value(&symbols).is_ok());
    let duplicate =
        json!({"symbol":[{"symbolId":1,"symbolName":"A"},{"symbolId":1,"symbolName":"B"}]});
    assert!(catalogue::decode(&duplicate).is_err());
    assert!(catalogue::decode(&json!({"symbol":{}})).is_err());
    assert!(
        catalogue::decode(
            &json!({"symbol":[{"symbolId":1,"symbolName":"AUDUSD","enabled":"true"}]})
        )
        .is_err()
    );
}

#[test]
fn fractional_milliseconds_preserve_half_open_time_semantics() {
    init();
    let start = DateTime::from_timestamp(60, 1).expect("fractional start");
    let end = DateTime::from_timestamp(120, 1).expect("fractional end");
    assert_eq!(
        ranges::timestamp_windows(NativePeriod::Minute1, start, end).expect("windows"),
        vec![(60001, 120001)]
    );
    let same_millisecond = DateTime::from_timestamp(60, 2).expect("fractional end");
    assert!(
        ranges::timestamp_windows(NativePeriod::Minute1, start, same_millisecond)
            .expect("no broker timestamp in range")
            .is_empty()
    );
}

#[test]
fn native_periods_match_protocol_numbers_without_inventing_unsupported_periods() {
    init();
    use NativePeriod::*;
    let periods = [
        Minute1, Minute2, Minute3, Minute4, Minute5, Minute10, Minute15, Minute30, Hour1, Hour4,
        Hour12, Day1, Week1, Month1,
    ];
    assert_eq!(
        periods.map(|period| period.wire().0),
        [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]
    );
    assert_eq!(NativePeriod::Month1.wire().1, "MN1");
}

#[tokio::test]
async fn fix_only_chart_history_is_refused_before_any_connection() {
    init();
    let quote = ctrader_fix::Config {
        host: "unresolvable.invalid".into(),
        port: 5211,
        sender_comp_id: "demo.example.123".into(),
        target_comp_id: "cServer".into(),
        sender_sub_id: "QUOTE".into(),
        password: "test-only".into(),
    };
    let result = ChartData::connect_profile("demo", access::Profile::Fix(quote)).await;
    let error = result.err().expect("FIX has no historical bars");
    assert!(error.to_string().contains("Open API OAuth"));
}

#[test]
fn native_windows_use_official_bar_limits_instead_of_tick_history_week_limit() {
    init();
    let day = 86_400_000;
    let monthly = ranges::windows(NativePeriod::Month1, 0, 45 * 365 * day).expect("45 years");
    assert_eq!(monthly.len(), 9);
    assert_eq!(monthly[0], (0, 1830 * day));
    let daily = ranges::windows(NativePeriod::Day1, 0, 500 * day).expect("500 days");
    assert_eq!(daily, vec![(0, 366 * day), (366 * day, 500 * day)]);
    let hourly = ranges::windows(NativePeriod::Hour1, 0, 501 * 3_600_000).expect("501 hours");
    assert_eq!(hourly.len(), 2);
    assert_eq!(hourly[0], (0, 500 * 3_600_000));
}

#[test]
fn catalogue_cache_expires_after_24_hours_without_extending_on_reads() {
    init();
    let created = std::time::Instant::now();
    let ttl = std::time::Duration::from_secs(24 * 60 * 60);
    assert!(catalogue_is_fresh(created, created));
    assert!(catalogue_is_fresh(
        created,
        created + ttl - std::time::Duration::from_nanos(1)
    ));
    assert!(!catalogue_is_fresh(created, created + ttl));
    assert!(!catalogue_is_fresh(
        created,
        created + ttl + std::time::Duration::from_secs(1)
    ));
}

fn symbol() -> InstrumentMetadata {
    InstrumentMetadata {
        id: 123,
        name: "EURUSD".into(),
        digits: 5,
        pip_size: 0.0001,
        min_units: 1000.0,
        step_units: 1000.0,
        max_units: 1_000_000.0,
        base_asset: 1,
        quote_asset: 2,
        raw: json!({}),
    }
}

fn page_response(minutes: impl IntoIterator<Item = i64>, more: bool) -> Value {
    let rows: Vec<_> = minutes
        .into_iter()
        .map(|minute| {
            let mut bar = row();
            bar["utcTimestampInMinutes"] = json!(minute);
            bar
        })
        .collect();
    let mut response = response(json!(rows));
    response["hasMore"] = json!(more);
    response
}

#[tokio::test]
async fn orchestrator_reads_truncated_pages_backwards_and_advances_windows_without_gaps() {
    init();
    let mut replies = std::collections::VecDeque::from([
        page_response([1], true),
        page_response([0], false),
        page_response([3, 2], false),
    ]);
    let mut requests = Vec::new();
    let bars = ranges::read_with(
        &symbol(),
        NativePeriod::Minute1,
        vec![(0, 120000), (120000, 240000)],
        |payload| {
            requests.push(payload);
            std::future::ready(Ok(replies.pop_front().expect("mock response")))
        },
    )
    .await
    .expect("complete history");
    assert_eq!(
        bars.iter()
            .map(|bar| bar.time.timestamp())
            .collect::<Vec<_>>(),
        vec![0, 60, 120, 180]
    );
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[0],
        json!({"symbolId":123,"period":1,"fromTimestamp":0,"toTimestamp":119999,"count":512})
    );
    assert_eq!(requests[1]["toTimestamp"], 59999);
    assert_eq!(requests[2]["fromTimestamp"], 120000);
    assert_eq!(requests[2]["toTimestamp"], 239999);
    assert!(replies.is_empty());
}

#[tokio::test]
async fn orchestrator_checks_full_page_without_has_more_and_accepts_complete_lower_boundary() {
    init();
    let mut replies = std::collections::VecDeque::from([
        page_response(1..=512, false),
        page_response([0], false),
    ]);
    let mut requests = Vec::new();
    let bars = ranges::read_with(
        &symbol(),
        NativePeriod::Minute1,
        vec![(0, 513 * 60000)],
        |payload| {
            requests.push(payload);
            std::future::ready(Ok(replies.pop_front().expect("mock response")))
        },
    )
    .await
    .expect("full page paginated");
    assert_eq!(bars.len(), 513);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["toTimestamp"], 59999);

    let mut count = 0;
    let bars = ranges::read_with(
        &symbol(),
        NativePeriod::Minute1,
        vec![(0, 512 * 60000)],
        |_| {
            count += 1;
            std::future::ready(Ok(page_response(0..512, false)))
        },
    )
    .await
    .expect("full page exactly reaches start");
    assert_eq!(bars.len(), 512);
    assert_eq!(count, 1);
}

#[tokio::test]
async fn orchestrator_propagates_later_read_failure_instead_of_returning_partial_history() {
    init();
    let mut requests = 0;
    let result = ranges::read_with(&symbol(), NativePeriod::Minute1, vec![(0, 120000)], |_| {
        requests += 1;
        std::future::ready(if requests == 1 {
            Ok(page_response([1], true))
        } else {
            Err(eyre!("mock transport disconnected"))
        })
    })
    .await;
    assert!(result.is_err());
    assert_eq!(requests, 2);
    assert!(
        result
            .expect_err("read failure")
            .to_string()
            .contains("mock transport disconnected")
    );
}

#[tokio::test]
async fn orchestrator_refuses_has_more_without_rows_and_overlarge_pages() {
    init();
    for response in [page_response([], true), page_response(0..513, false)] {
        let result = ranges::read_with(
            &symbol(),
            NativePeriod::Minute1,
            vec![(0, 514 * 60000)],
            |_| std::future::ready(Ok(response.clone())),
        )
        .await;
        assert!(result.is_err());
    }
}
