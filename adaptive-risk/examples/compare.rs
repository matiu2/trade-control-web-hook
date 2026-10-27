use adaptive_risk::{simulation::*, *};
use color_eyre::eyre::{Result, ensure, eyre};
use serde::Deserialize;
use tracing_subscriber::prelude::*;
#[derive(Deserialize)]
struct Records {
    dataset: Dataset,
    trades: Vec<TradeRecord>,
}
fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()
        .ok();
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("monte-carlo");
    let policy: RiskPolicy = std::env::var_os("RISK_POLICY_FILE")
        .map(|file| -> Result<RiskPolicy> { Ok(toml::from_str(&std::fs::read_to_string(file)?)?) })
        .transpose()?
        .unwrap_or_default();
    policy.validate()?;
    let output = match command {
        "monte-carlo" => {
            let config: MonteCarloConfig = if let Some(file) = args.get(2) {
                serde_json::from_slice(&std::fs::read(file)?)?
            } else {
                MonteCarloConfig::default()
            };
            ensure!(
                config.simulations >= 10000,
                "reports require at least 10,000 simulations per strategy"
            );
            let records: Option<Records> = args
                .get(3)
                .map(|file| -> Result<Records> {
                    Ok(serde_json::from_slice(&std::fs::read(file)?)?)
                })
                .transpose()?;
            let results = monte_carlo(
                &policy,
                &config,
                records.as_ref().map(|r| (r.trades.as_slice(), r.dataset)),
            )?;
            serde_json::to_string_pretty(
                &serde_json::json!({"policy":policy,"simulation_config":config,"timestamp_model":"configured_trades_per_day","sampler":if records.is_some(){"circular_chronological_blocks"}else{"provisional_binary"},"confidence_level":0.95,"results":results}),
            )?
        }
        "replay" => {
            let file = args
                .get(2)
                .ok_or_else(|| eyre!("replay requires provenance-tagged records JSON"))?;
            let records: Records = serde_json::from_slice(&std::fs::read(file)?)?;
            let results = compare(
                &policy,
                &records.trades,
                records.dataset,
                Decimal::from(10000),
                Decimal::from(10),
            )?;
            serde_json::to_string_pretty(
                &serde_json::json!({"policy":policy,"sequential_completed_setups":true,"results":results}),
            )?
        }
        _ => {
            return Err(eyre!(
                "usage: compare [monte-carlo [config.json [records.json]] | replay records.json]"
            ));
        }
    };
    println!("{output}");
    Ok(())
}
