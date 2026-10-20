use super::*;
pub(super) async fn account(
    broker: &CtraderBroker,
    req: &EntryRequest<'_>,
    symbol: &InstrumentMetadata,
) -> std::result::Result<Value, EntryError> {
    let assets = metadata::assets(broker)
        .await
        .map_err(|_| EntryError::AccountFetch)?;
    let fx = |id| {
        assets
            .iter()
            .find(|a| integer(a, "assetId").ok() == Some(id))
            .and_then(|a| a["name"].as_str())
            .is_some_and(metadata::currency)
    };
    if !fx(symbol.base_asset) || !fx(symbol.quote_asset) {
        return Err(reject(
            "cTrader risk sizing currently supports recognized spot FX currencies only",
        ));
    }
    if symbol.raw.get("tradingMode").is_some() && !is_enum(&symbol.raw, "tradingMode", 0, "ENABLED")
    {
        return Err(reject("cTrader symbol is not enabled for entries"));
    }
    let trader = metadata::trader(broker)
        .await
        .map_err(|_| EntryError::AccountFetch)?;
    if !is_enum(&trader, "accountType", 0, "HEDGED") && trader.get("accountType").is_some() {
        return Err(reject(
            "cTrader netted/spread-betting accounts are not supported",
        ));
    }
    if trader["isLimitedRisk"].as_bool() == Some(true) {
        return Err(reject(
            "cTrader limited-risk accounts require guaranteed-stop support",
        ));
    }
    if trader.get("accessRights").is_some() && !is_enum(&trader, "accessRights", 0, "FULL_ACCESS") {
        return Err(reject("cTrader account does not have full trading rights"));
    }
    if req.direction == Direction::Short
        && symbol.raw["enableShortSelling"].as_bool() == Some(false)
    {
        return Err(reject("cTrader symbol does not permit short selling"));
    }
    if !fx(integer(&trader, "depositAssetId").map_err(|_| EntryError::AccountFetch)?) {
        return Err(reject(
            "cTrader account deposit asset is not a supported FX currency",
        ));
    }
    Ok(trader)
}
