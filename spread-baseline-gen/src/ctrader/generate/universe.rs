use broker_ctrader::market_data::ChartData;
use color_eyre::{Result, eyre::ensure};

pub(super) async fn select(
    data: &ChartData,
    only: &[String],
) -> Result<(Vec<String>, Vec<crate::mt5::Failure>)> {
    let catalogue = data.catalogue().await?;
    ensure!(
        only.iter().all(|s| catalogue
            .iter()
            .any(|r| r.name == *s && r.enabled != Some(false))),
        "requested instrument missing or disabled in broker catalogue"
    );
    let mut requested = Vec::new();
    let mut excluded = Vec::new();
    for symbol in catalogue
        .iter()
        .filter(|s| s.enabled != Some(false) && (only.is_empty() || only.contains(&s.name)))
    {
        match super::measure::asset_for_fx(&symbol.name) {
            Ok(_) => match data.forex_instrument_metadata(&symbol.name).await {
                Ok(_) => requested.push(symbol.name.clone()),
                Err(error)
                    if only.is_empty()
                        && format!("{error:#}").contains("not supported spot FX") =>
                {
                    tracing::info!(symbol=symbol.name, %error, "excluded unsupported broker asset legs");
                    excluded.push(crate::mt5::Failure {
                        symbol: symbol.name.clone(),
                        error: format!("{error:#}"),
                    });
                }
                Err(error) => return Err(error.wrap_err("verify supported broker FX catalogue")),
            },
            Err(error) if only.is_empty() => {
                tracing::info!(symbol=symbol.name, %error, "excluded from supported FX calibration universe");
                excluded.push(crate::mt5::Failure {
                    symbol: symbol.name.clone(),
                    error: format!("{error:#}"),
                });
            }
            Err(error) => return Err(error),
        }
    }
    requested.sort();
    requested.dedup();
    ensure!(
        !requested.is_empty(),
        "account has no supported FX instruments"
    );
    Ok((requested, excluded))
}
