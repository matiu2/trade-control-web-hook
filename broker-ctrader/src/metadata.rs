use super::*;
use wire::{array, integer, money, number};

#[derive(Debug, Clone)]
pub struct InstrumentMetadata {
    pub name: String,
    pub id: i64,
    pub digits: u32,
    pub pip_size: f64,
    pub min_units: f64,
    pub step_units: f64,
    pub max_units: f64,
    pub(crate) base_asset: i64,
    pub(crate) quote_asset: i64,
    pub(crate) raw: Value,
}
pub(crate) async fn symbols(broker: &OpenApiBroker) -> Result<Vec<Value>> {
    Ok(array(
        &broker
            .read(2114, 2115, json!({"includeArchivedSymbols":false}))
            .await?,
        "symbol",
    )
    .to_vec())
}
fn normalize(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_uppercase)
        .collect()
}
pub(crate) async fn instrument(broker: &OpenApiBroker, name: &str) -> Result<InstrumentMetadata> {
    let symbols = symbols(broker).await?;
    let exact = symbols
        .iter()
        .find(|s| s["symbolName"].as_str() == Some(name));
    let candidates: Vec<_> = symbols
        .iter()
        .filter(|s| {
            s["symbolName"]
                .as_str()
                .is_some_and(|n| normalize(n) == normalize(name))
        })
        .collect();
    let light = exact
        .or_else(|| (candidates.len() == 1).then(|| candidates[0]))
        .ok_or_else(|| eyre!("cTrader symbol {name} missing or ambiguous on selected account"))?;
    let id = integer(light, "symbolId")?;
    let data = broker.read(2116, 2117, json!({"symbolId":[id]})).await?;
    let full = array(&data, "symbol")
        .iter()
        .find(|s| integer(s, "symbolId").ok() == Some(id))
        .ok_or_else(|| eyre!("cTrader full symbol metadata unavailable"))?;
    let digits = integer(full, "digits")?;
    let pip = integer(full, "pipPosition")?;
    ensure!(
        (0..=8).contains(&digits) && (0..=8).contains(&pip),
        "invalid cTrader price precision"
    );
    Ok(InstrumentMetadata {
        name: light["symbolName"]
            .as_str()
            .ok_or_else(|| eyre!("symbol has no name"))?
            .into(),
        id,
        digits: digits as u32,
        pip_size: 10f64.powi(-(pip as i32)),
        min_units: number(full, "minVolume")? / 100.0,
        step_units: number(full, "stepVolume")? / 100.0,
        max_units: number(full, "maxVolume")? / 100.0,
        base_asset: integer(light, "baseAssetId")?,
        quote_asset: integer(light, "quoteAssetId")?,
        raw: full.clone(),
    })
}
pub(crate) async fn trader(broker: &OpenApiBroker) -> Result<Value> {
    let data = broker.read(2121, 2122, json!({})).await?;
    let t = data["trader"].clone();
    ensure!(
        integer(&t, "ctidTraderAccountId")? == broker.account_id(),
        "wrong cTrader trader account"
    );
    Ok(t)
}
pub(crate) async fn equity(broker: &OpenApiBroker) -> Result<f64> {
    let trader = trader(broker).await?;
    let data = broker.read(2187, 2188, json!({})).await?;
    wire::array_shape(&data, "positionUnrealizedPnL")?;
    let digits = integer(&data, "moneyDigits")?;
    ensure!((0..=12).contains(&digits), "invalid PnL moneyDigits");
    let pnl = array(&data, "positionUnrealizedPnL")
        .iter()
        .map(|p| number(p, "netUnrealizedPnL"))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .sum::<f64>()
        / 10f64.powi(digits as i32);
    let result = money(&trader, "balance")? + pnl;
    ensure!(
        result.is_finite() && result > 0.0,
        "cTrader account equity is not positive"
    );
    Ok(result)
}
pub(crate) async fn assets(broker: &OpenApiBroker) -> Result<Vec<Value>> {
    Ok(array(&broker.read(2112, 2113, json!({})).await?, "asset").to_vec())
}
pub(crate) async fn quote(broker: &OpenApiBroker, instrument: &str) -> Result<Quote> {
    let symbol = instrument_metadata_id(broker, instrument).await?;
    broker.shared.quote(symbol).await
}

pub(crate) async fn instrument_metadata_id(broker: &OpenApiBroker, name: &str) -> Result<i64> {
    Ok(instrument(broker, name).await?.id)
}
pub(crate) fn currency(name: &str) -> bool {
    [
        "USD", "EUR", "JPY", "GBP", "AUD", "NZD", "CAD", "CHF", "SGD", "HKD", "NOK", "SEK", "DKK",
        "PLN", "CZK", "HUF", "ZAR", "MXN", "TRY", "CNH", "CNY",
    ]
    .contains(&name)
}
