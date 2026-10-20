use broker_ctrader::access::Profile;
use color_eyre::eyre::{Result, ensure, eyre};
use trade_control_core::{
    account::AccountMetadata,
    broker::{Broker, Quote},
};

pub(super) async fn check(meta: &AccountMetadata, instrument: &str) -> Result<Quote> {
    let profile = Profile::load(&meta.name)?;
    profile.validate(meta.kind.is_live(), meta.oanda_account_id.as_deref())?;
    match profile {
        Profile::OpenApi(_) => {
            let broker = trade_control_worker::acquire_ctrader(meta).await?;
            let symbol = broker.instrument_metadata(instrument).await?;
            let equity = broker.equity().await?;
            println!(
                "cTrader Open API authenticated: equity={equity}; symbol={} pip_size={} min_units={} step_units={}",
                symbol.name, symbol.pip_size, symbol.min_units, symbol.step_units
            );
            broker
                .get_quote(instrument)
                .await
                .map_err(|error| eyre!("cTrader quote failed: {error:?}"))
        }
        Profile::Fix(config) => fix_quote(config, instrument).await,
    }
}

async fn fix_quote(config: ctrader_fix::Config, instrument: &str) -> Result<Quote> {
    let mut client = ctrader_fix::market_data::Client::connect(config).await?;
    let result = async {
        let requested = instrument.replace(['/', '_'], "").to_ascii_uppercase();
        let symbols = client.symbols().await?;
        let matches = symbols.iter().filter(|symbol| symbol.name.eq_ignore_ascii_case(&requested)).collect::<Vec<_>>();
        ensure!(matches.len() == 1, "cTrader FIX symbol must match exactly one discovered instrument");
        let symbol = matches[0];
        client.subscribe(symbol).await?;
        let quote = client.next_quote().await?;
        println!("cTrader FIX QUOTE authenticated: symbol={} id={} digits={}; quotes-only (no execution, positions, equity or candle history)", symbol.name, symbol.id, symbol.digits);
        Ok(Quote { bid: quote.bid, ask: quote.ask })
    }.await;
    let logout = client.logout().await;
    match (result, logout) {
        (Ok(quote), Ok(())) => Ok(quote),
        (Err(error), logout) => {
            if let Err(logout_error) = logout {
                tracing::warn!(%logout_error, "FIX probe logout failed after read failure");
            }
            Err(error)
        }
        (Ok(_), Err(error)) => Err(error),
    }
}
