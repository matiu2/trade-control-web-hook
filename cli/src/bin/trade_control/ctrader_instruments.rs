//! Account-specific, read-only Open API instrument discovery.
use super::BrokerKind;
use broker_ctrader::{
    access::Profile,
    market_data::{ChartData, InstrumentSummary},
};
use clap::ValueEnum;
use color_eyre::eyre::{Result, ensure, eyre};
use serde_json::json;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub(super) enum BrokerArg {
    Oanda,
    TradeNation,
    Ibkr,
    Ctrader,
    #[value(name = "ctrader-demo")]
    CtraderDemo,
}

impl BrokerArg {
    pub(super) fn select(self, account: Option<String>) -> Result<(BrokerKind, Option<String>)> {
        let broker = match self {
            Self::Oanda => BrokerKind::Oanda,
            Self::TradeNation => BrokerKind::TradeNation,
            Self::Ibkr => BrokerKind::Ibkr,
            Self::Ctrader => BrokerKind::Ctrader,
            Self::CtraderDemo => {
                ensure!(
                    account.as_deref().is_none_or(|name| name == "ctrader-demo"),
                    "--broker ctrader-demo selects --account ctrader-demo; conflicting --account is not allowed"
                );
                return Ok((BrokerKind::Ctrader, Some("ctrader-demo".into())));
            }
        };
        Ok((broker, account))
    }
}

pub(super) enum Operation {
    List { json: bool },
    Refresh,
    Resolve { name: String, json: bool },
}

pub(super) fn run(operation: Operation, account: Option<&str>) -> Result<()> {
    let account = require_account(account)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run_connected(operation, account))
}

fn require_account(account: Option<&str>) -> Result<&str> {
    account
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            eyre!("cTrader instrument discovery requires --account with a named Open API profile")
        })
}

fn profile_environment(account: &str, profile: &Profile) -> Result<serde_json::Value> {
    let environment = match profile {
        Profile::OpenApi(config)
        | Profile::Hybrid {
            open_api: config, ..
        } => serde_json::to_value(config.environment)?,
        _ => {
            return Err(eyre!(
                "cTrader instrument discovery requires an Open API profile; FIX supplies quotes only"
            ));
        }
    };
    ensure!(
        account != "ctrader-demo" || environment == "demo",
        "ctrader-demo must use a demo Open API account"
    );
    Ok(environment)
}

async fn run_connected(operation: Operation, account: &str) -> Result<()> {
    let profile = Profile::load(account)?;
    let environment = profile_environment(account, &profile)?;
    let reader = ChartData::connect_profile(account, profile).await?;
    // Each CLI invocation starts a fresh process/session, so this request fetches
    // the broker catalogue anew, including for `refresh`. No global catalogue
    // can supply an account-local cTrader symbol ID.
    let symbols = reader.catalogue().await?;
    match operation {
        Operation::List { json: true } => println!(
            "{}",
            serde_json::json!({
                "broker":"ctrader", "account":account, "profile":account, "environment":environment,
                "ctid_trader_account_id":reader.account_id(), "instruments":symbols,
            })
        ),
        Operation::List { json: false } => symbols
            .iter()
            .for_each(|symbol| println!("{}", symbol.name)),
        Operation::Refresh => println!(
            "refreshed: {} instruments for {} (cTrader account {}; account-local Open API catalogue)",
            symbols.len(),
            account,
            reader.account_id(),
        ),
        Operation::Resolve { name, json } => {
            resolve_print(&reader, account, &environment, &symbols, &name, json).await?
        }
    }
    Ok(())
}

fn normalize(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_uppercase)
        .collect()
}

fn resolve<'a>(symbols: &'a [InstrumentSummary], query: &str) -> Option<&'a InstrumentSummary> {
    if let Some(exact) = symbols.iter().find(|symbol| symbol.name == query) {
        return Some(exact);
    }
    let key = normalize(query);
    if key.is_empty() {
        return None;
    }
    let mut candidates = symbols
        .iter()
        .filter(|symbol| normalize(&symbol.name) == key);
    let candidate = candidates.next()?;
    candidates.next().is_none().then_some(candidate)
}

async fn resolve_print(
    reader: &ChartData,
    account: &str,
    environment: &serde_json::Value,
    symbols: &[InstrumentSummary],
    name: &str,
    json: bool,
) -> Result<()> {
    let Some(symbol) = resolve(symbols, name) else {
        print_miss(account, reader.account_id(), name, json);
    };
    let metadata = reader.instrument_metadata(&symbol.name).await?;
    ensure!(
        metadata.id == symbol.id && metadata.name == symbol.name,
        "cTrader instrument identity changed while resolving; retry catalogue read"
    );
    if json {
        println!(
            "{}",
            json!({
                "ok": true, "broker": "ctrader", "account": account, "profile":account, "environment":environment,
                "ctid_trader_account_id": reader.account_id(), "name": metadata.name,
                "symbol_id": metadata.id, "symbol": metadata.name,
                "description": symbol.description, "enabled": symbol.enabled,
                "digits": metadata.digits, "pip_size": metadata.pip_size,
                "min_units": metadata.min_units, "step_units": metadata.step_units,
                "max_units": metadata.max_units,
            })
        );
    } else {
        println!("name:       {}", metadata.name);
        println!("symbol_id:  {}", metadata.id);
        println!("account:    {account}");
        println!("min_units:  {}", metadata.min_units);
        println!("step_units: {}", metadata.step_units);
        println!("max_units:  {}", metadata.max_units);
    }
    Ok(())
}

fn print_miss(account: &str, account_id: i64, query: &str, json: bool) -> ! {
    if json {
        println!(
            "{}",
            json!({
                "ok": false, "broker": "ctrader", "account": account,
                "ctid_trader_account_id": account_id, "query": query, "candidates": [],
            })
        );
    } else {
        eprintln!("no unambiguous cTrader instrument match for {query:?} on account {account}");
    }
    std::process::exit(2);
}

#[cfg(test)]
#[path = "ctrader_instruments/tests.rs"]
mod tests;
