//! Price stop-loss risk in the deposit currency, using USD only if no direct pair exists.
use super::{EntryError, OpenApiBroker, Quote, Value, integer, metadata};
use std::future::Future;

#[cfg(test)]
mod tests;

type ConversionResult<T> = std::result::Result<T, EntryError>;

pub(super) async fn rate(broker: &OpenApiBroker, from: i64, to: i64) -> ConversionResult<f64> {
    if from == to {
        return Ok(1.0);
    }
    let symbols = metadata::symbols(broker)
        .await
        .map_err(|error| failure(from, to, format!("conversion symbol lookup: {error}")))?;
    rate_with(
        from,
        to,
        &symbols,
        || async {
            metadata::assets(broker)
                .await
                .map_err(|error| failure(from, to, format!("conversion asset lookup: {error}")))
        },
        |name| async move {
            metadata::quote(broker, name).await.map_err(|error| {
                failure(
                    from,
                    to,
                    format!("conversion quote {name} unavailable: {error}"),
                )
            })
        },
    )
    .await
}

async fn rate_with<'a, Assets, Quotes>(
    from: i64,
    to: i64,
    symbols: &'a [Value],
    assets: impl FnOnce() -> Assets,
    mut quote: impl FnMut(&'a str) -> Quotes,
) -> ConversionResult<f64>
where
    Assets: Future<Output = ConversionResult<Vec<Value>>>,
    Quotes: Future<Output = ConversionResult<Quote>>,
{
    if from == to {
        return Ok(1.0);
    }
    if let Some(leg) = direct(symbols, from, to)? {
        return leg.rate(quote(leg.name).await?);
    }
    let usd = usd_asset(&assets().await?, from, to)?;
    let first = required_leg(symbols, from, usd)?;
    let second = required_leg(symbols, usd, to)?;
    let rate = first.rate(quote(first.name).await?)? * second.rate(quote(second.name).await?)?;
    valid_rate(rate, from, to)
}

fn failure(from: i64, to: i64, detail: impl Into<String>) -> EntryError {
    EntryError::CurrencyConversion {
        from: from.to_string(),
        to: to.to_string(),
        detail: detail.into(),
    }
}

fn usd_asset(assets: &[Value], from: i64, to: i64) -> ConversionResult<i64> {
    let usd: Vec<_> = assets
        .iter()
        .filter(|asset| asset["name"].as_str() == Some("USD"))
        .collect();
    match usd.as_slice() {
        [asset] => integer(asset, "assetId")
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| failure(from, to, "USD asset ID missing or invalid")),
        _ => Err(failure(
            from,
            to,
            "no unique USD asset for cross conversion",
        )),
    }
}

struct Leg<'a> {
    name: &'a str,
    from: i64,
    to: i64,
    inverse: bool,
}

fn direct(symbols: &[Value], from: i64, to: i64) -> ConversionResult<Option<Leg<'_>>> {
    let pairs: Vec<_> = symbols
        .iter()
        .filter(|symbol| {
            let base = integer(symbol, "baseAssetId").ok();
            let quote = integer(symbol, "quoteAssetId").ok();
            (base == Some(from) && quote == Some(to)) || (base == Some(to) && quote == Some(from))
        })
        .collect();
    match pairs.as_slice() {
        [] => Ok(None),
        [pair] => Ok(Some(Leg {
            name: pair["symbolName"]
                .as_str()
                .ok_or_else(|| failure(from, to, "conversion pair symbol name missing"))?,
            from,
            to,
            inverse: integer(pair, "baseAssetId").ok() != Some(from),
        })),
        _ => Err(failure(from, to, "ambiguous conversion pair")),
    }
}

fn required_leg(symbols: &[Value], from: i64, to: i64) -> ConversionResult<Leg<'_>> {
    direct(symbols, from, to)?.ok_or_else(|| {
        failure(
            from,
            to,
            "no direct conversion pair for USD cross conversion",
        )
    })
}

impl Leg<'_> {
    fn rate(&self, quote: Quote) -> ConversionResult<f64> {
        if ![quote.bid, quote.ask]
            .iter()
            .all(|v| v.is_finite() && *v > 0.0)
            || quote.ask < quote.bid
        {
            return Err(failure(self.from, self.to, "invalid conversion quote"));
        }
        // Preserve the direct sizing convention: ask forward, inverse bid backward.
        let rate = if self.inverse {
            1.0 / quote.bid
        } else {
            quote.ask
        };
        valid_rate(rate, self.from, self.to)
    }
}

fn valid_rate(rate: f64, from: i64, to: i64) -> ConversionResult<f64> {
    if !rate.is_finite() || rate <= 0.0 {
        return Err(failure(from, to, "invalid conversion rate"));
    }
    Ok(rate)
}
