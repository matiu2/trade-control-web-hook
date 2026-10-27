use broker_ctrader::market_data::ChartData;
use color_eyre::{Result, eyre::ensure};

pub(super) async fn select(
    data: &ChartData,
    only: &[String],
    all_instruments: bool,
    stocks: bool,
) -> Result<(Vec<String>, Vec<crate::mt5::Failure>)> {
    validate_scope(only, stocks)?;
    if stocks {
        return select_stocks(data, only).await;
    }
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
        let selected = if only.is_empty() {
            super::super::calibration::discovery_asset(&symbol.name, all_instruments)
        } else {
            super::super::calibration::asset(&symbol.name, true)
        };
        match selected {
            Ok(_) => {
                match super::super::calibration::metadata(
                    data,
                    &symbol.name,
                    !only.is_empty() || all_instruments,
                )
                .await
                {
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
                    Err(error) => {
                        return Err(error.wrap_err("verify supported broker instrument catalogue"));
                    }
                }
            }
            Err(error) if only.is_empty() => {
                tracing::info!(symbol=symbol.name, %error, "excluded from supported calibration universe");
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
        "account has no supported instruments for this calibration selection"
    );
    Ok((requested, excluded))
}

fn validate_scope(only: &[String], stocks: bool) -> Result<()> {
    let native = broker_ctrader::catalogue::stock_symbols()?;
    ensure!(
        only.iter()
            .all(|name| native.contains(&name.as_str()) == stocks),
        "--stocks requires only audited native stocks; stock --only selection requires --stocks"
    );
    Ok(())
}

async fn select_stocks(
    data: &ChartData,
    only: &[String],
) -> Result<(Vec<String>, Vec<crate::mt5::Failure>)> {
    let mut requested = if only.is_empty() {
        broker_ctrader::catalogue::stock_symbols()?
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
    } else {
        only.to_vec()
    };
    requested.sort();
    ensure!(
        requested.windows(2).all(|pair| pair[0] != pair[1]),
        "duplicate native stock selection"
    );
    for symbol in &requested {
        data.verified_stock_metadata(symbol).await?;
    }
    Ok((requested, vec![]))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stocks_are_a_separate_exact_scope_and_cannot_mix_with_nonstocks() {
        crate::ctrader::test_logging();
        validate_scope(&[], true).unwrap();
        validate_scope(&["AIA".into(), "REDES ENERGETICAS NACIONAIS".into()], true).unwrap();
        validate_scope(&["EURUSD".into()], false).unwrap();
        assert!(validate_scope(&["AIA".into()], false).is_err());
        assert!(validate_scope(&["EURUSD".into()], true).is_err());
        assert!(validate_scope(&["AIA".into(), "EURUSD".into()], true).is_err());
        assert!(validate_scope(&["aia".into()], true).is_err());
        assert_eq!(
            broker_ctrader::catalogue::stock_symbols().unwrap().len(),
            350
        );
        assert_eq!(
            broker_ctrader::catalogue::non_stock_symbols()
                .unwrap()
                .len(),
            93
        );
    }
}
