use super::*;
use serde_json::{Value, json};

fn init_tracing() {
    use tracing_subscriber::prelude::*;
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    });
}

fn plan() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/plan_export.json")).unwrap()
}

fn recorded_timeline() -> Value {
    serde_json::from_str(include_str!("../../tests/fixtures/plan_timeline.json")).unwrap()
}

#[test]
fn recorded_fixture_decodes_with_shared_worker_types() {
    init_tracing();
    serde_json::from_value::<trade_control_core::recording::PlanTimeline>(recorded_timeline())
        .expect("recorded worker fixture remains compatible");
}

#[test]
fn show_retains_state_settlement_and_unknown_fields() {
    init_tracing();
    let plan = plan();
    let id = plan["trade_id"].as_str().unwrap();
    let raw = json!([{"plan":plan,"state":{"phase":"done"},"settlement":{"future_field":42},"extra":"retained"}]);
    let stored = StoredPlan::from_value(id, raw.clone()).unwrap();
    assert_eq!(stored.raw, raw);
    assert_eq!(stored.plan["trade_id"], id);
    assert_eq!(stored.detail.trade_id, id);
    assert_eq!(stored.raw[0]["settlement"]["future_field"], 42);
    assert!(serde_json::to_value(stored).is_ok());
}

#[test]
fn show_rejects_ambiguous_or_wrong_trade_instead_of_picking_first() {
    init_tracing();
    let plan = plan();
    let id = plan["trade_id"].as_str().unwrap();
    assert!(StoredPlan::from_value("different", json!([{"plan":plan}])).is_err());
    assert!(StoredPlan::from_value(id, json!([{"plan":plan},{"plan":plan}])).is_err());
}

#[test]
fn settlement_summary_belongs_to_the_selected_plan() {
    init_tracing();
    let plan = plan();
    let id = plan["trade_id"].as_str().unwrap();
    let raw = json!([
        {"plan":{"trade_id":"another"},"settlement":{"currency":"UNRELATED"}},
        {"plan":plan}
    ]);
    let stored = StoredPlan::from_value(id, raw).unwrap();
    assert!(stored.settlement_lines.is_empty());
}

#[test]
fn timeline_retains_server_logs_and_unrendered_evidence() {
    init_tracing();
    let raw = json!({"records":[{"ts":"2026-07-23T00:00:00Z","request_id":"request-1","method":"POST","path":"/","headers":[],"body":"recorded body","intent_id":null,"trade_id":"trade-1","status":200,"outcome":"entered: 1","logs":[{"level":"log","msg":"server evidence"}]}],"ticks":[],"notes":[],"future_field":true});
    let timeline = RecordedTimeline::from_value(raw.clone()).unwrap();
    assert_eq!(timeline.raw, raw);
    assert_eq!(timeline.events.len(), 1);
    assert_eq!(timeline.entry_ts.as_deref(), Some("2026-07-23 10:00"));
    assert!(serde_json::to_value(timeline).is_ok());
}

#[test]
fn timeline_hides_inspection_requests_but_retains_their_raw_logs() {
    init_tracing();
    let actions = [
        "plan-show",
        "plan-timeline",
        "register",
        "prep",
        "veto",
        "close",
    ];
    let records: Vec<_> = actions
        .iter()
        .map(|action| {
            json!({
                "ts":"2026-10-28T21:08:00Z", "request_id":action,
                "method":"POST", "path":"/", "headers":[],
                "body":format!("action: {action}\ntrade_id: trade-1\n"),
                "intent_id":null, "trade_id":"trade-1", "status":200,
                "outcome":"recorded", "logs":[{"level":"log","msg":"server evidence"}]
            })
        })
        .collect();
    let raw = json!({"records":records,"ticks":[],"notes":[]});
    let timeline = RecordedTimeline::from_value(raw.clone()).unwrap();
    assert_eq!(timeline.raw, raw);
    let visible: Vec<_> = timeline
        .events
        .iter()
        .map(|event| event.text.as_str())
        .collect();
    assert_eq!(
        visible,
        [
            "register → recorded",
            "prep → recorded",
            "veto → recorded",
            "close → recorded"
        ]
    );
}

#[test]
fn malformed_timeline_is_an_error_instead_of_an_empty_success() {
    init_tracing();
    assert!(RecordedTimeline::from_value(json!({"error":"unavailable"})).is_err());
    assert!(RecordedTimeline::from_value(json!({"ticks":"corrupt"})).is_err());
    assert!(RecordedTimeline::from_value(json!({"records":[],"ticks":[]})).is_ok());
}

#[test]
fn timeline_requires_records_and_ticks_but_notes_remain_optional() {
    init_tracing();
    [
        json!({"records":[]}),
        json!({"ticks":[]}),
        json!({"notes":[]}),
    ]
    .into_iter()
    .for_each(|raw| assert!(RecordedTimeline::from_value(raw).is_err()));
    assert!(RecordedTimeline::from_value(json!({"records":[],"ticks":[]})).is_ok());
}

#[test]
fn malformed_recorded_entries_never_become_zero_fires() {
    init_tracing();
    let mutations = [
        ("/ticks/0", Value::Null),
        ("/ticks/0/eval/fired", json!("corrupt")),
        ("/ticks/0/eval/fired/0", json!({"rule_id":"incomplete"})),
        ("/records/0", Value::Null),
        ("/records/0/logs", json!([null])),
    ];
    mutations.into_iter().for_each(|(path, replacement)| {
        let mut raw = recorded_timeline();
        *raw.pointer_mut(path).unwrap() = replacement;
        assert!(RecordedTimeline::from_value(raw.clone()).is_err(), "{path}");
        assert!(
            compare_timeline_with_replay(&raw, "Done: true | final phase: Done | fires: 0")
                .is_err(),
            "{path}"
        );
    });
    assert!(RecordedTimeline::from_value(json!({"records":[],"ticks":[],"notes":[null]})).is_err());
}

#[test]
fn comparison_without_available_ticks_is_unavailable() {
    init_tracing();
    let error = compare_timeline_with_replay(
        &json!({"records":[],"ticks":[]}),
        "Done: true | final phase: Done | fires: 0",
    )
    .unwrap_err();
    assert!(error.to_string().contains("unavailable"));
}

#[test]
fn failed_or_incomplete_replay_summary_cannot_get_a_comparison() {
    init_tracing();
    [
        "Done: false | error: data-unavailable | detail: no candles | Net R: n/a",
        "Done: false | final phase: Done | fires: 0 | error: failed | Net R: n/a",
        "Done: true",
        "Done: true | final phase: Done",
        "Done: true | final phase: | fires: 0",
    ]
    .into_iter()
    .for_each(|report| {
        assert!(
            compare_timeline_with_replay(&recorded_timeline(), report).is_err(),
            "{report}"
        )
    });
}

#[test]
fn valid_nonterminal_zero_fire_report_can_be_compared_to_a_recorded_tick() {
    init_tracing();
    let mut raw = recorded_timeline();
    raw["ticks"] = json!([raw["ticks"][0].clone()]);
    raw["ticks"][0]["eval"]["fired"] = json!([]);
    raw["ticks"][0]["dispatch_outcomes"] = json!([]);
    let comparison =
        compare_timeline_with_replay(&raw, "Done: false | final phase: AwaitEntry | fires: 0")
            .unwrap();
    assert!(comparison.divergences.is_clean());
}

#[test]
fn list_response_requires_a_body_but_accepts_a_real_empty_sequence() {
    init_tracing();
    assert!(parse_list_response("").is_err());
    assert!(parse_list_response(" \n\t").is_err());
    assert!(parse_list_response("[]").unwrap().is_empty());
}

#[test]
fn malformed_replay_cannot_claim_clean_comparison() {
    init_tracing();
    assert!(
        compare_timeline_with_replay(&json!({"records":[],"ticks":[]}), "failed to replay")
            .is_err()
    );
}

#[test]
fn comparison_uses_selected_report_and_serializes() {
    init_tracing();
    let raw: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/plan_timeline.json")).unwrap();
    let comparison = compare_timeline_with_replay(
        &raw,
        "Done: true | final phase: done | fires: 0 | TP: 0 SL: 0 | Net R: 0",
    )
    .unwrap();
    assert!(!comparison.live_fires.is_empty());
    assert!(comparison.replay_fires.is_empty());
    assert!(!comparison.divergences.is_clean());
    assert!(serde_json::to_value(comparison).is_ok());
}

#[test]
fn worker_url_requires_explicit_http_destination_without_credentials() {
    init_tracing();
    assert!(worker_url("file:///tmp/key").is_err());
    assert!(worker_url("https://user:secret@example.com").is_err());
    assert!(worker_url("http://127.0.0.1:8788").is_ok());
}

#[test]
fn signed_requests_are_read_operations_with_unique_ids() {
    init_tracing();
    use trade_control_core::intent::Action;
    let now = Utc::now();
    let list = build_plan_list_intent(now, &request_suffix().unwrap(), true);
    let show = build_plan_show_intent("trade-1", now, &request_suffix().unwrap());
    let timeline = build_plan_timeline_intent("trade-1", now, &request_suffix().unwrap());
    assert_eq!(list.action, Action::PlanList);
    assert_eq!(show.action, Action::PlanShow);
    assert_eq!(timeline.action, Action::PlanTimeline);
    assert_ne!(list.id, show.id);
    let signed = wrap_signed(&list, &[7; KEY_LEN], now).unwrap();
    assert!(signed.contains("sig: v1-sig.") || signed.contains("sig: \"v1-sig."));
}
