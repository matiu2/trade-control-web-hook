//! Unsupported currencies and unavailable contract metadata are distinct.
use super::*;
use std::collections::BTreeMap;

pub(super) fn validate(base: i64, quote: i64, assets: &[Value]) -> Result<()> {
    let mut names = BTreeMap::new();
    for asset in assets {
        let id = wire::integer(asset, "assetId")?;
        let name = asset["name"]
            .as_str()
            .filter(|name| !name.trim().is_empty() && !name.chars().any(char::is_control))
            .ok_or_else(|| eyre!("invalid cTrader asset name metadata"))?;
        ensure!(
            id > 0 && names.insert(id, name).is_none(),
            "invalid or duplicate cTrader asset metadata"
        );
    }
    let supported = [base, quote]
        .into_iter()
        .map(|id| {
            names
                .get(&id)
                .copied()
                .map(metadata::currency)
                .ok_or_else(|| eyre!("missing referenced cTrader asset metadata"))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        supported.into_iter().all(|currency| currency),
        "cTrader instrument is not supported spot FX"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_assets_cannot_be_classified_as_unsupported_forex() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
        let assets = vec![
            json!({"assetId":1,"name":"EUR"}),
            json!({"assetId":2,"name":"USD"}),
        ];
        assert!(validate(1, 2, &assets).is_ok());
        let mut gold = assets.clone();
        gold[1]["name"] = json!("XAU");
        assert!(
            validate(1, 2, &gold)
                .unwrap_err()
                .to_string()
                .contains("not supported spot FX")
        );
        for rows in [
            vec![],
            vec![json!({"assetId":1})],
            vec![json!({"assetId":"broken","name":"EUR"})],
            vec![assets[0].clone(), assets[0].clone()],
        ] {
            let error = validate(1, 2, &rows).unwrap_err().to_string();
            assert!(!error.contains("not supported spot FX"), "{error}");
        }
    }
}
