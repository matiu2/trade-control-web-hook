//! Offline enrollment/status and read-only reconciliation. No order submission command.
use broker_ctrader::protection::{PropAccount, monitor};
use color_eyre::eyre::{Result, eyre};
use std::collections::BTreeMap;
use tracing_subscriber::prelude::*;
#[derive(serde::Deserialize)]
struct Profiles {
    accounts: BTreeMap<String, PropAccount>,
}
#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()
        .ok();
    let args: Vec<_> = std::env::args().collect();
    let command=args.get(1).ok_or_else(||eyre!("usage: prop-risk-admin enroll|status profiles.toml account [day-reference.json] OR monitor account"))?;
    if command == "monitor" {
        return monitor(args.get(2).ok_or_else(|| eyre!("account required"))?).await;
    }
    let path = args.get(2).ok_or_else(|| eyre!("profiles TOML required"))?;
    let account = args.get(3).ok_or_else(|| eyre!("account required"))?;
    let mut profiles: Profiles = toml::from_str(&std::fs::read_to_string(path)?)?;
    let profile = profiles
        .accounts
        .remove(account)
        .ok_or_else(|| eyre!("unknown account"))?;
    match command.as_str() {
        "enroll" => {
            let reference = args
                .get(4)
                .ok_or_else(|| eyre!("verified day-reference JSON required"))?;
            profile.enroll(serde_json::from_slice(&std::fs::read(reference)?)?)?;
        }
        "status" => {
            let session = profile.manager()?.begin()?;
            println!("{}", serde_json::to_string_pretty(session.state())?);
        }
        _ => return Err(eyre!("unknown command")),
    }
    Ok(())
}
