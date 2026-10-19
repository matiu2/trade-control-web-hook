//! Convert profit-currency risk using current executable bid/ask quotes.
use color_eyre::{
    Result,
    eyre::{WrapErr, ensure, eyre},
};
use mt5_data_source::{
    quotes::Quote,
    web::{Symbol, trading::Trader},
};
use std::future::Future;
use trade_control_core::broker::EntryError;

#[cfg(test)]
mod tests;

pub(crate) async fn rate(trader: &Trader, from: &str, to: &str) -> Result<f64, EntryError> {
    if from == to {
        return Ok(1.0);
    }
    let result = async {
        let symbols = trader.symbols().await?;
        convert(&symbols, from, to, |name| async move {
            trader.quote(&name).await
        })
        .await
    }
    .await;
    result.map_err(|error| {
        tracing::warn!(from, to, error = %format!("{error:#}"), "MT5 risk currency conversion failed before submission");
        EntryError::CurrencyConversion {
            from: from.into(), to: to.into(), detail: format!("{error:#}"),
        }
    })
}

async fn convert<F, Fut>(symbols: &[Symbol], from: &str, to: &str, mut read: F) -> Result<f64>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Quote>>,
{
    // Select the entire route before reading any quotes. Missing pairs are
    // ordinary catalog absences, not failed connections to probe or retry.
    let mut rate = 1.0;
    for leg in route(symbols, from, to)? {
        let quote = read(leg.symbol.clone())
            .await
            .wrap_err_with(|| format!("conversion quote for {}", leg.symbol))?;
        quote
            .validate()
            .wrap_err_with(|| format!("invalid conversion quote for {}", leg.symbol))?;
        // Paying quote-currency costs uses ask; inverse costs use 1 / bid.
        rate *= if leg.inverse {
            1.0 / quote.bid
        } else {
            quote.ask
        };
    }
    ensure!(rate.is_finite() && rate > 0.0, "invalid conversion rate");
    Ok(rate)
}

struct Leg {
    symbol: String,
    inverse: bool,
}

fn route(symbols: &[Symbol], from: &str, to: &str) -> Result<Vec<Leg>> {
    if from == to {
        return Ok(Vec::new());
    }
    if let Some(leg) = listed_leg(symbols, from, to) {
        return Ok(vec![leg]);
    }
    [(from, "USD"), ("USD", to)]
        .into_iter()
        .map(|(from, to)| listed_leg(symbols, from, to))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            eyre!("no listed conversion route from {from} to {to}, directly or through USD")
        })
}

fn listed_leg(symbols: &[Symbol], from: &str, to: &str) -> Option<Leg> {
    [
        (format!("{from}{to}"), false),
        (format!("{to}{from}"), true),
    ]
    .into_iter()
    .find(|(name, _)| symbols.iter().any(|s| s.name == *name))
    .map(|(symbol, inverse)| Leg { symbol, inverse })
}
