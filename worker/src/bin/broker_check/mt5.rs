use chrono::{Timelike, Utc};
use color_eyre::{Result, eyre::{ensure, eyre}};
use trade_control_core::{account::AccountMetadata, broker::{Broker, EntryRequest, Granularity}, intent::{Direction, ResolvedEntry, RiskBudget}};

pub async fn check(broker: &broker_mt5::Mt5Broker, meta: &AccountMetadata, symbol: &str, count: u32, preview: bool) -> Result<()> {
    if count > 0 {
        ensure!(count <= 300, "MT5 probe candle count must be <= 300");
        let now = Utc::now();
        let hour = now.with_minute(0).and_then(|t| t.with_second(0)).and_then(|t| t.with_nanosecond(0)).ok_or_else(|| eyre!("invalid candle boundary"))?;
        let since = hour - chrono::Duration::hours(i64::from(count)+1);
        let bars = broker.get_bidask_candles(symbol,Granularity::H1,since,now).await?;
        ensure!(bars.len() == count as usize, "MT5 probe received {} candles, requested {count}", bars.len());
        tracing::info!(symbol, count, "MT5 EA candles verified through the server broker adapter");
    }
    if preview {
        let trader = mt5_data_source::account_config::AccountConfig::load(&meta.name)?.trader().await?;
        let contract = trader.symbol(symbol).await?;
        let q = broker.get_quote(symbol).await?;
        let pip = contract.point * if matches!(contract.digits,3|5) {10.0}else{1.0};
        let req = EntryRequest { instrument:symbol, direction:Direction::Short, entry:ResolvedEntry::Market{reference_price:q.bid}, stop_loss:q.ask+20.0*pip, take_profit:q.bid-40.0*pip,
            risk:RiskBudget::Units(contract.rules.min_volume_raw as f64 / mt5_data_source::web::trading::Lots::SCALE as f64), dry_run:true, contract_multiplier:None };
        let placement = broker.place_entry(meta.caps.resolve_max_risk_pct(1.0), meta.caps.resolve_max_open_positions(100), &req).await?;
        ensure!(placement.order_id == "dry-run", "MT5 preview returned an unexpected identity");
        tracing::info!(?placement, "MT5 minimum-lot short preview passed; no order transmitted");
    }
    Ok(())
}
