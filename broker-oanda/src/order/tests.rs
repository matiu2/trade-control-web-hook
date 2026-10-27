use super::*;
use trade_control_core::intent::RiskBudget;

pub(crate) fn logging() {
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
}

#[test]
fn real_orders_preserve_fractional_units_price_precision_and_protective_stops() {
    logging();
    let spec = Spec {
        price_places: 1,
        unit_places: 3,
        minimum: 0.001,
        maximum: 100.0,
    };
    for (entry, kind) in [
        (
            ResolvedEntry::Market {
                reference_price: 10000.1,
            },
            "MARKET",
        ),
        (
            ResolvedEntry::Stop {
                trigger_price: 10000.1,
            },
            "STOP",
        ),
        (
            ResolvedEntry::Limit {
                trigger_price: 10000.1,
            },
            "LIMIT",
        ),
    ] {
        for direction in [Direction::Long, Direction::Short] {
            let request = EntryRequest {
                instrument: "BTC_USD",
                direction,
                entry: entry.clone(),
                stop_loss: 9990.2,
                take_profit: 10020.3,
                risk: RiskBudget::Units(0.012),
                dry_run: false,
                contract_multiplier: None,
            };
            let value = serde_json::to_value(build(&request, &spec, 0.012)).unwrap();
            assert_eq!(value["type"], kind);
            assert_eq!(
                value["units"],
                if direction == Direction::Long {
                    "0.012"
                } else {
                    "-0.012"
                }
            );
            assert_eq!(value["stopLossOnFill"]["price"], "9990.2");
            assert_eq!(value["takeProfitOnFill"]["price"], "10020.3");
            if kind != "MARKET" {
                assert_eq!(value["price"], "10000.1");
            }
        }
    }
}
