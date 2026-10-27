//! Select broker and account without letting chart inference override explicit choices.
use color_eyre::eyre::{Result, eyre};
use trade_control_conventions::{Broker, split_symbol};

pub(super) fn broker(
    explicit: Option<Broker>,
    environment: Option<&str>,
    chart_symbol: &str,
) -> Result<Broker> {
    if let Some(broker) = explicit {
        return Ok(broker);
    }
    if let Some(name) = environment.map(str::trim).filter(|name| !name.is_empty()) {
        return Broker::from_wire(name)
            .ok_or_else(|| eyre!("unsupported TRADE_CONTROL_BROKER {name:?}"));
    }
    if local_chart_client::mt5_chart_account(chart_symbol).is_some() {
        return Ok(Broker::Mt5);
    }
    if local_chart_client::ctrader_chart_account(chart_symbol).is_some() {
        return Ok(Broker::Ctrader);
    }
    if chart_symbol
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("ctrader-"))
    {
        return Err(eyre!(
            "invalid named cTrader chart identity {chart_symbol:?}; \
             expected ctrader-<account>:<symbol>"
        ));
    }
    let (exchange, _) = split_symbol(chart_symbol);
    Ok(exchange
        .and_then(Broker::from_exchange)
        .unwrap_or(Broker::Oanda))
}

pub(super) fn account(
    explicit: Option<&str>,
    environment: Option<&str>,
    broker: Broker,
    chart_symbol: &str,
) -> Result<String> {
    if let Some(account) = explicit {
        return Ok(account.to_string());
    }
    if let Some(name) = environment.map(str::trim).filter(|name| !name.is_empty()) {
        return Ok(name.to_string());
    }
    let chart_account = match broker {
        Broker::Mt5 => local_chart_client::mt5_chart_account(chart_symbol),
        Broker::Ctrader => local_chart_client::ctrader_chart_account(chart_symbol),
        _ => None,
    };
    if let Some(account) = chart_account {
        return Ok(account.to_string());
    }
    broker
        .default_account_index()
        .map(str::to_string)
        .ok_or_else(|| {
            eyre!(
                "{} has no default account, so one must be named explicitly: \
                 pass --account-id <name> or set TRADE_CONTROL_ACCOUNT",
                broker.as_str()
            )
        })
}

#[cfg(test)]
mod tests;
