//! Explicitly attach one already-filled cTrader market entry; preview by default.
use clap::Parser;
use color_eyre::eyre::{Result, ensure, eyre};
use trade_control_core::{account::MetadataStore, intent::BrokerKind, sig::parse_key_hex};
use trade_control_worker::{
    Config, PgMetadataStore, PgStateStore, acquire_ctrader,
    entry_reconciliation::{FillProof, persist, recover},
    recording_pg::request_records_for_trade,
};

#[derive(Parser)]
#[command(name = "trade-control-reconcile-entry")]
#[command(
    about = "Attach an existing proven demo fill, keeping its current broker SL/TP; read-only unless --apply"
)]
struct Cli {
    #[arg(long)]
    account: String,
    #[arg(long)]
    trade_id: String,
    #[arg(long)]
    audit_id: String,
    /// Commit management tracking and resolve this audit after broker verification.
    /// Never sends an order, closes a position, or changes SL/TP.
    #[arg(long)]
    apply: bool,
    #[arg(long)]
    config: Option<String>,
    #[arg(long, env = "DATABASE_URL")]
    database_url: Option<String>,
    /// Read the original webhook signing key from this private file instead of SIGNING_KEY.
    #[arg(long)]
    key_file: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()
        .ok();
    let cli = Cli::parse();
    run(cli).await
}

async fn run(cli: Cli) -> Result<()> {
    let key = signing_key(&cli)?;
    let store = PgStateStore::connect(&database_url(&cli)?)
        .await
        .map_err(|error| eyre!("database connection failed: {error}"))?;
    let meta = PgMetadataStore::from_state_store(&store)
        .get(&cli.account)
        .await
        .map_err(|error| eyre!("account metadata lookup failed: {error}"))?;
    ensure!(
        meta.broker == BrokerKind::Ctrader && !meta.kind.is_live(),
        "this operator currently supports cTrader demo accounts only"
    );
    let broker = acquire_ctrader(&meta).await?;
    let guard = broker.prepare_market_reconciliation(&cli.audit_id).await?;
    let proof: FillProof = serde_json::from_value(serde_json::to_value(guard.proof())?)?;
    ensure!(
        proof.account == cli.account,
        "broker proof refers to a different account"
    );
    let records = request_records_for_trade(store.pool(), &cli.trade_id).await?;
    let adoption = recover(&records, &key, &cli.trade_id, proof, chrono::Utc::now())?;
    if !cli.apply {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"mode":"preview","committed":false,"adoption":adoption})
            )?
        );
        return Ok(());
    }
    let receipt = persist(store.pool(), &adoption, chrono::Utc::now(), guard.refresh()).await?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    guard.resolve(receipt).await.map_err(|error| eyre!("tracking was committed; broker audit resolution failed, rerun the same command to finish without another order: {error}"))?;
    Ok(())
}

fn signing_key(cli: &Cli) -> Result<[u8; 32]> {
    let text = match &cli.key_file {
        Some(path) => {
            std::fs::read_to_string(path).map_err(|_| eyre!("unable to read signing key file"))?
        }
        None => std::env::var("SIGNING_KEY").map_err(|_| {
            eyre!("SIGNING_KEY or --key-file is required to authenticate the original request")
        })?,
    };
    parse_key_hex(&text).map_err(|_| eyre!("signing key must be 64 hexadecimal characters"))
}

fn database_url(cli: &Cli) -> Result<String> {
    if let Some(url) = &cli.database_url {
        return Ok(url.clone());
    }
    if let Some(path) = &cli.config {
        return load_database_config(path);
    }
    let home_config = std::env::var("HOME")
        .ok()
        .map(|home| format!("{home}/.config/trade-control/trade-control.toml"));
    let path = home_config
        .iter()
        .map(String::as_str)
        .chain(std::iter::once("./trade-control.toml"))
        .find(|path| std::path::Path::new(path).is_file())
        .ok_or_else(|| eyre!("no database configuration: provide --config or DATABASE_URL"))?;
    load_database_config(path)
}

fn load_database_config(path: &str) -> Result<String> {
    Config::load(path)
        .map(|config| config.database.url)
        .map_err(|_| eyre!("unable to load database configuration from {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_requires_explicit_apply_flag() {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer())
            .try_init()
            .ok();
        let args = [
            "reconcile",
            "--account",
            "ctrader-demo",
            "--trade-id",
            "test-trade",
            "--audit-id",
            "audit-123",
        ];
        assert!(!Cli::try_parse_from(args).unwrap().apply);
        assert!(
            Cli::try_parse_from(args.into_iter().chain(["--apply"]))
                .unwrap()
                .apply
        );
    }
}
