use super::*;
use std::collections::VecDeque;
use tracing_subscriber::prelude::*;

pub(super) struct Script {
    pub(super) replies: VecDeque<Value>,
    pub(super) mutations: Vec<(u32, Value)>,
    pub(super) notes: Vec<String>,
    pub(super) fail_amend: bool,
    pub(super) disconnect: bool,
    pub(super) initial: Value,
    pub(super) amended_volume: i64,
    pub(super) stall: bool,
    pub(super) audit_failure: Option<String>,
    pub(super) amended_override: Option<Value>,
}
impl Transport for Script {
    async fn read(&mut self, _kind: u32, _response: u32, _payload: Value) -> Result<Value> {
        ensure!(!self.disconnect, "connection closed after submission");
        if self.stall {
            return std::future::pending().await;
        }
        self.replies
            .pop_front()
            .ok_or_else(|| color_eyre::eyre::eyre!("script exhausted"))
    }
    async fn mutate(
        &mut self,
        kind: u32,
        payload: Value,
    ) -> std::result::Result<Value, ctrader_api::RequestError> {
        self.mutations.push((kind, payload));
        if kind == 2106 {
            Ok(self.initial.clone())
        } else if self.fail_amend {
            Err(ctrader_api::RequestError::Rejected {
                code: "INVALID_STOPS".into(),
            })
        } else {
            Ok(
                json!({"ctidTraderAccountId":7,"position":self.amended_override.clone().unwrap_or_else(|| position(self.amended_volume,true))}),
            )
        }
    }
    fn note(&mut self, phase: &str, _evidence: &Value) -> Result<()> {
        self.notes.push(phase.into());
        ensure!(
            self.audit_failure.as_deref() != Some(phase),
            "audit persistence unavailable"
        );
        Ok(())
    }
}
pub(super) fn init() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}
pub(super) fn terms() -> Terms {
    init();
    Terms {
        account: 7,
        symbol: 8,
        volume: 100000,
        client_id: "audit-1".into(),
        direction: Direction::Long,
        stop: 1.099,
        target: 1.102,
        requested_price: 1.1,
        max_stop_distance: 0.0015,
        before: BTreeSet::new(),
    }
}
pub(super) fn detail(status: i64, filled: i64) -> Value {
    json!({"ctidTraderAccountId":7,"order":{"orderId":9,"positionId":11,"clientOrderId":"audit-1",
        "orderType":1,"orderStatus":status,"executedVolume":filled,"tradeData":{"symbolId":8,"tradeSide":1,"volume":100000}},
        "deal":if filled > 0 {vec![json!({"dealId":15,"orderId":9,"positionId":11,"symbolId":8,"tradeSide":1,"dealStatus":2,"filledVolume":filled,"executionPrice":1.1002})]} else {vec![]}})
}
pub(super) fn position(filled: i64, normalized: bool) -> Value {
    json!({"positionId":11,"positionStatus":1,"price":1.1002,
        "tradeData":{"symbolId":8,"tradeSide":1,"volume":filled},
        "stopLoss":if normalized {1.099} else {1.0992},"takeProfit":if normalized {1.102} else {1.1022}})
}
pub(super) fn snapshot(filled: i64, normalized: bool) -> Value {
    json!({"ctidTraderAccountId":7,"order":[],"position":[position(filled,normalized)]})
}
pub(super) fn script(replies: Vec<Value>) -> Script {
    Script {
        replies: replies.into(),
        mutations: vec![],
        notes: vec![],
        fail_amend: false,
        disconnect: false,
        initial: json!({"ctidTraderAccountId":7,"order":{"orderId":9},"executionType":2}),
        amended_volume: 100000,
        stall: false,
        audit_failure: None,
        amended_override: None,
    }
}
pub(super) fn successful() -> Script {
    script(vec![
        detail(1, 0),
        detail(2, 100000),
        snapshot(100000, false),
        snapshot(100000, true),
    ])
}

pub(super) fn short_case() -> (Terms, Script) {
    let mut t = terms();
    t.direction = Direction::Short;
    t.stop = 1.101;
    t.target = 1.098;
    let mut order = detail(2, 100000);
    order["order"]["tradeData"]["tradeSide"] = json!(2);
    order["deal"][0]["executionPrice"] = json!(1.0998);
    order["deal"][0]["tradeSide"] = json!(2);
    let mut before = snapshot(100000, false);
    let mut after = snapshot(100000, true);
    for s in [&mut before, &mut after] {
        s["position"][0]["tradeData"]["tradeSide"] = json!(2);
        s["position"][0]["price"] = json!(1.0998);
    }
    before["position"][0]["stopLoss"] = json!(1.1008);
    before["position"][0]["takeProfit"] = json!(1.0978);
    after["position"][0]["stopLoss"] = json!(1.101);
    after["position"][0]["takeProfit"] = json!(1.098);
    let mut io = script(vec![order, before, after.clone()]);
    io.amended_override = Some(after["position"][0].clone());
    (t, io)
}
