//! Fresh account-native stock joins for read-only calibration.
use super::*;
use crate::catalogue::{NativeStockMetadata, StockMetadata};

impl ChartData {
    /// One fresh account check permits planning the audited stock universe.
    /// Every symbol still requires verified_stock_metadata before reading candles.
    pub async fn verify_stock_account(&self) -> Result<()> {
        crate::catalogue::verify_stock_account(
            &self.broker.account_name,
            &self.account_identity().await?,
        )
    }

    /// Verify the exact account, native identity, precision and declared sessions.
    /// An enabled or close-only stock can supply history; this never permits orders.
    pub async fn verified_stock_metadata(&self, symbol: &str) -> Result<InstrumentMetadata> {
        let identity = self.account_identity().await?;
        crate::catalogue::verify_stock_account(&self.broker.account_name, &identity)?;
        let lights = metadata::symbols(&self.broker).await?;
        let light = exact_symbol(&lights, symbol)?;
        let metadata = metadata::instrument(&self.broker, symbol).await?;
        let assets = metadata::assets(&self.broker).await?;
        let categories = self.broker.read(2160, 2161, json!({})).await?;
        let classes = self.broker.read(2153, 2154, json!({})).await?;
        wire::array_shape(&categories, "symbolCategory")?;
        wire::array_shape(&classes, "assetClass")?;
        let category = unique_id(
            wire::array(&categories, "symbolCategory"),
            "id",
            wire::integer(light, "symbolCategoryId")?,
        )?;
        let native = StockMetadata::from_native(NativeStockMetadata {
            light,
            full_symbol: &metadata.raw,
            base_asset: unique_id(&assets, "assetId", wire::integer(light, "baseAssetId")?)?,
            quote_asset: unique_id(&assets, "assetId", wire::integer(light, "quoteAssetId")?)?,
            category,
            asset_class: unique_id(
                wire::array(&classes, "assetClass"),
                "id",
                wire::integer(category, "assetClassId")?,
            )?,
        })?;
        ensure!(
            metadata.name == symbol && metadata.id == native.id,
            "native stock symbol changed during verification"
        );
        crate::catalogue::verify_stock_metadata(&self.broker.account_name, &identity, &native)?;
        Ok(metadata)
    }
}

fn exact_symbol<'a>(rows: &'a [Value], name: &str) -> Result<&'a Value> {
    let mut matches = rows
        .iter()
        .filter(|row| row["symbolName"].as_str() == Some(name));
    let row = matches
        .next()
        .ok_or_else(|| eyre!("exact native stock {name:?} is missing"))?;
    ensure!(matches.next().is_none(), "ambiguous native stock {name:?}");
    Ok(row)
}

fn unique_id<'a>(rows: &'a [Value], key: &str, id: i64) -> Result<&'a Value> {
    ensure!(id > 0, "invalid native stock metadata join ID");
    let ids = rows
        .iter()
        .map(|row| Ok((row, wire::integer(row, key)?)))
        .collect::<Result<Vec<_>>>()?;
    let mut matches = ids.iter().filter(|(_, actual)| *actual == id);
    let row = matches
        .next()
        .ok_or_else(|| eyre!("missing native stock metadata {key}={id}"))?;
    ensure!(
        matches.next().is_none(),
        "ambiguous native stock metadata {key}={id}"
    );
    Ok(row.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logging() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
    }

    #[test]
    fn native_stock_joins_refuse_aliases_missing_duplicates_and_malformed_ids() {
        logging();
        let row = json!({"symbolName":"AIA", "assetId":7});
        assert_eq!(
            exact_symbol(std::slice::from_ref(&row), "AIA").unwrap(),
            &row
        );
        for alias in ["aia", "AIA GROUP LTD", "A.I.A"] {
            assert!(exact_symbol(std::slice::from_ref(&row), alias).is_err());
        }
        assert!(exact_symbol(&[row.clone(), row.clone()], "AIA").is_err());
        assert!(unique_id(&[], "assetId", 7).is_err());
        assert!(unique_id(std::slice::from_ref(&row), "assetId", 0).is_err());
        assert!(unique_id(&[row.clone(), row], "assetId", 7).is_err());
        assert!(unique_id(&[json!({"assetId":7}), json!({})], "assetId", 7).is_err());
        assert_eq!(
            unique_id(&[json!({"assetId":"7"})], "assetId", 7).unwrap()["assetId"],
            "7"
        );
    }
}
