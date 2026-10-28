use super::*;
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use tracing_subscriber::prelude::*;
use trade_control_core::intent::OversizePolicy;

fn init() {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

fn symbol() -> InstrumentMetadata {
    InstrumentMetadata {
        name: "AUDCHF".into(),
        id: 23,
        digits: 5,
        pip_size: 0.0001,
        min_units: 1000.0,
        step_units: 1000.0,
        max_units: 10_000_000.0,
        base_asset: 1,
        quote_asset: 2,
        raw: json!({"minVolume":100000,"maxVolume":1000000000,"stepVolume":100000,"lotSize":10000000}),
    }
}

fn request(risk: RiskBudget) -> EntryRequest<'static> {
    EntryRequest {
        setup_id: Some("ihs-audchf-06967b08"),
        instrument: "AUDCHF",
        direction: Direction::Long,
        entry: ResolvedEntry::Limit {
            trigger_price: 0.58365,
        },
        stop_loss: 0.58313,
        take_profit: 0.58576,
        risk,
        on_oversize: OversizePolicy::CapVolume,
        dry_run: true,
        contract_multiplier: None,
    }
}

fn sized(req: &EntryRequest<'_>) -> std::result::Result<SizedEntry, EntryError> {
    calculate(
        "ctrader-demo",
        req,
        &symbol(),
        0.58365,
        1_000_000.0,
        1.0 / 0.58365,
        1.0,
    )
}

#[test]
fn percent_amount_and_unit_budgets_cap_with_lower_cash_risk_and_original_bracket() {
    init();
    for risk in [
        RiskBudget::Percent(1.0),
        RiskBudget::Amount(10_000.0),
        RiskBudget::Units(11_000_000.0),
    ] {
        let req = request(risk);
        let sized = sized(&req).unwrap();
        assert_eq!(sized.cents, 1_000_000_000);
        let cash_risk = 10_000_000.0 * (0.58365 - 0.58313) / 0.58365;
        assert!((sized.implied_pct - cash_risk / 1_000_000.0 * 100.0).abs() < 1e-10);
        assert!(sized.implied_pct < 1.0);
        let payload = geometry::payload(42, 23, sized.cents, &req, 0.58365).unwrap();
        assert_eq!(payload["volume"], 1_000_000_000);
        assert_eq!(payload["stopLoss"], 0.58313);
        assert_eq!(payload["takeProfit"], 0.58576);
    }
}

#[test]
fn explicit_reject_refuses_oversize_but_permits_smaller_trades() {
    init();
    let mut req = request(RiskBudget::Percent(1.0));
    req.on_oversize = OversizePolicy::Reject;
    let error = sized(&req).err().unwrap().to_string();
    assert!(error.contains("on_oversize=reject"));
    req.risk = RiskBudget::Percent(0.5);
    assert!(sized(&req).unwrap().cents < 1_000_000_000);
}

#[test]
fn capping_never_hides_a_budget_above_the_account_risk_cap() {
    init();
    for risk in [
        RiskBudget::Percent(2.0),
        RiskBudget::Amount(20_000.0),
        RiskBudget::Units(20_000_000.0),
    ] {
        assert!(matches!(
            sized(&request(risk)),
            Err(EntryError::RiskCapExceeded { .. })
        ));
    }
}

#[test]
fn short_and_market_entries_keep_their_protection_geometry() {
    init();
    for direction in [Direction::Long, Direction::Short] {
        let mut req = request(RiskBudget::Percent(1.0));
        req.direction = direction;
        if direction == Direction::Short {
            req.stop_loss = 0.58417;
            req.take_profit = 0.58154;
        }
        req.entry = ResolvedEntry::Market {
            reference_price: 0.58365,
        };
        let sized = sized(&req).unwrap();
        assert_eq!(sized.cents, 1_000_000_000);
        let payload = geometry::payload(42, 23, sized.cents, &req, 0.58365).unwrap();
        assert_eq!(payload["relativeStopLoss"], 52);
        assert_eq!(payload["relativeTakeProfit"], 211);
        assert!(sized.max_stop_distance >= 0.00052);
    }
}

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn oversize_warns_with_account_requested_size_actual_risk_and_preserved_prices() {
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(move || writer.clone()),
        );
    tracing::subscriber::with_default(subscriber, || {
        assert!(sized(&request(RiskBudget::Percent(1.0))).is_ok());
        assert!(sized(&request(RiskBudget::Percent(0.5))).is_ok());
    });
    let output = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert_eq!(output.matches("WARN").count(), 1, "{output}");
    for field in [
        "ctrader-demo",
        "AUDCHF",
        "requested_units=",
        "maximum_units=10000000",
        "capped_units=10000000",
        "requested_risk_pct=1",
        "capped_risk_pct=",
        "stop_loss=0.58313",
        "take_profit=0.58576",
    ] {
        assert!(output.contains(field), "missing {field}: {output}");
    }
}
