use super::*;
use std::collections::HashSet;

pub(super) async fn read(broker: &OpenApiBroker) -> Result<Vec<InstrumentSummary>> {
    let data = broker
        .read(2114, 2115, json!({"includeArchivedSymbols": false}))
        .await?;
    decode(&data)
}

pub(super) fn decode(data: &Value) -> Result<Vec<InstrumentSummary>> {
    wire::array_shape(data, "symbol")?;
    let mut seen = HashSet::new();
    let mut symbols = wire::array(data, "symbol")
        .iter()
        .map(|symbol| {
            let id = wire::integer(symbol, "symbolId")?;
            ensure!(
                id > 0 && seen.insert(id),
                "invalid or duplicate cTrader catalogue symbol ID"
            );
            let name = symbol["symbolName"]
                .as_str()
                .ok_or_else(|| eyre!("cTrader catalogue symbol has no name"))?;
            ensure!(
                !name.trim().is_empty() && !name.chars().any(char::is_control),
                "invalid cTrader catalogue symbol name"
            );
            let description = optional_text(symbol, "description")?;
            let enabled = symbol
                .get("enabled")
                .map(|value| {
                    value
                        .as_bool()
                        .ok_or_else(|| eyre!("invalid cTrader symbol enabled flag"))
                })
                .transpose()?;
            Ok(InstrumentSummary {
                id,
                name: name.into(),
                description,
                enabled,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    symbols.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    Ok(symbols)
}

fn optional_text(row: &Value, field: &str) -> Result<Option<String>> {
    row.get(field)
        .map(|value| {
            value
                .as_str()
                .map(String::from)
                .ok_or_else(|| eyre!("invalid cTrader catalogue {field}"))
        })
        .transpose()
}
