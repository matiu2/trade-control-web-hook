//! Read-only sizing regression; the request is explicitly a dry run.
use broker_mt5::Mt5Broker;
use color_eyre::{Result, eyre::ensure};
use mt5_data_source::account_config::AccountConfig;
use tracing_subscriber::prelude::*;
use trade_control_core::{
    broker::{Broker, EntryRequest},
    intent::{Direction, ResolvedEntry, RiskBudget},
};

#[tokio::test]
#[ignore = "read-only AUDJPY sizing preview; requires a configured USD demo account and network"]
async fn audjpy_minimum_lot_preview_uses_the_listed_inverse_conversion_pair() -> Result<()> {
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .try_init()
        .ok();
    let name = std::env::var("MT5_BROKER_TEST_ACCOUNT")?;
    let trader = AccountConfig::load(&name)?.trader().await?;
    let account = trader.account().await?;
    ensure!(
        account.currency == "USD",
        "regression requires a USD account"
    );
    let contract = trader.symbol("AUDJPY").await?;
    ensure!(
        contract.profit_currency == "JPY",
        "regression requires JPY risk"
    );
    let symbols = trader.symbols().await?;
    ensure!(
        !symbols.iter().any(|s| s.name == "JPYUSD") && symbols.iter().any(|s| s.name == "USDJPY"),
        "regression requires USDJPY listed and JPYUSD absent"
    );
    let quote = trader.quote("AUDJPY").await?;
    let lots =
        contract.rules.min_volume_raw as f64 / mt5_data_source::web::trading::Lots::SCALE as f64;
    let request = EntryRequest {
        on_oversize: Default::default(),
        setup_id: None,
        instrument: "AUDJPY",
        direction: Direction::Long,
        entry: ResolvedEntry::Market {
            reference_price: quote.ask,
        },
        stop_loss: quote.bid - 0.2,
        take_profit: quote.ask + 0.4,
        risk: RiskBudget::Units(lots),
        dry_run: true,
        contract_multiplier: None,
    };
    let broker = Mt5Broker::connect(&name).await?;
    let placement = broker.place_entry(1.0, 100, &request).await?;
    ensure!(placement.order_id == "dry-run", "expected a sizing preview");
    ensure!(
        placement.size == Some(lots),
        "expected the minimum lot size"
    );
    tracing::info!(
        ?placement,
        "AUDJPY sizing preview passed; no order submitted"
    );
    Ok(())
}
