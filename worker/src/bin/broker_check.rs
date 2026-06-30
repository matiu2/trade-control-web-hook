//! `trade-control-broker-check` — verify an account's broker session is live.
//!
//! The worker's broker-acquisition path (`acquire_tn` / `acquire_oanda`) is the
//! one thing the unit tests *can't* cover — they only assert the broker-mismatch
//! guard, never a real login (no network in CI). So the first time it runs for
//! real is on a live demo trade, which is a bad place to discover the enc store
//! is missing an entry or the OANDA token is wrong.
//!
//! This probe closes that gap: given an account **name**, it resolves the
//! metadata from Postgres (exactly as the dispatch path does), acquires the
//! broker the same way (`acquire_tn` reads the enc store by name; `acquire_oanda`
//! uses `OANDA_API_KEY` + the recorded sub-account id), and does **one cheap
//! read** (`get_quote`) to prove the session is genuinely live. No order is ever
//! placed — read-only.
//!
//! Use it as a pre-flight before the first demo trade, and as a VM deploy sanity
//! check. Same DB-URL resolution as `trade-control-accounts`.
//!
//! ```text
//! # OANDA needs the token in env; TradeNation reads the enc store.
//! OANDA_API_KEY=… trade-control-broker-check testing --instrument EUR/USD
//! ```

use clap::Parser;
use color_eyre::eyre::{Result, eyre};

use trade_control_core::account::{MetadataError, MetadataStore};
use trade_control_core::broker::Broker;
use trade_control_core::intent::BrokerKind;
use trade_control_worker::{
    Config, PgMetadataStore, PgStateStore, Secrets, acquire_oanda, acquire_tn,
};

#[derive(Parser)]
#[command(name = "trade-control-broker-check")]
#[command(about = "Verify a named account's broker session is live (read-only — no orders placed)")]
struct Cli {
    /// The account name (must exist in the `accounts` table, and — for
    /// TradeNation — in the enc store under the same name).
    account: String,

    /// Instrument to quote as the liveness check. Use the broker's own symbol
    /// form (TradeNation `EUR/USD`, OANDA `EUR_USD`).
    #[arg(long, default_value = "EUR/USD")]
    instrument: String,

    /// Path to a `trade-control.toml` for `database.url`. Omitted → same search
    /// as `trade-control-accounts` (`~/.config/trade-control/` then `./`).
    #[arg(long)]
    config: Option<String>,

    /// Postgres URL override. Highest precedence.
    #[arg(long, env = "DATABASE_URL")]
    database_url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    let db_url = resolve_db_url(&cli)?;
    let store = PgStateStore::connect(&db_url)
        .await
        .map_err(|e| eyre!("connecting to Postgres: {e}"))?;
    let accounts = PgMetadataStore::from_state_store(&store);

    let meta = match accounts.get(&cli.account).await {
        Ok(m) => m,
        Err(MetadataError::NotFound(_)) => {
            return Err(eyre!(
                "no account '{}' in the index — add it with trade-control-accounts",
                cli.account
            ));
        }
        Err(e) => return Err(eyre!("account lookup failed: {e}")),
    };
    println!(
        "account '{}' → broker={} kind={:?}",
        meta.name,
        broker_str(meta.broker),
        meta.kind
    );

    // Acquire the broker exactly as the worker does, then one cheap read.
    let quote = match meta.broker {
        BrokerKind::TradeNation => {
            println!("acquiring TradeNation session from the enc store (by name)…");
            let broker = acquire_tn(&meta)
                .await
                .map_err(|e| eyre!("acquire_tn failed: {e}"))?;
            broker.get_quote(&cli.instrument).await
        }
        BrokerKind::Oanda => {
            println!("acquiring OANDA client (OANDA_API_KEY + sub-account)…");
            let secrets = Secrets::from_env().map_err(|e| eyre!("loading secrets: {e}"))?;
            let broker =
                acquire_oanda(&meta, &secrets).map_err(|e| eyre!("acquire_oanda failed: {e}"))?;
            broker.get_quote(&cli.instrument).await
        }
    };

    match quote {
        Ok(q) => {
            println!(
                "OK — live quote for {}: bid={} ask={} (spread={:.5})",
                cli.instrument,
                q.bid,
                q.ask,
                q.spread()
            );
            println!("broker session is LIVE — this account can place demo trades.");
            Ok(())
        }
        Err(e) => Err(eyre!(
            "session acquired but quote read failed for {}: {e:?} — \
             the login worked but the broker may not list this instrument; \
             try --instrument with a symbol the broker trades",
            cli.instrument
        )),
    }
}

fn broker_str(b: BrokerKind) -> &'static str {
    match b {
        BrokerKind::Oanda => "oanda",
        BrokerKind::TradeNation => "tradenation",
    }
}

/// DB-URL resolution mirroring `trade-control-accounts`: `--database-url` /
/// `DATABASE_URL` > `--config` > `~/.config/trade-control/trade-control.toml` >
/// `./trade-control.toml`.
fn resolve_db_url(cli: &Cli) -> Result<String> {
    if let Some(url) = &cli.database_url {
        return Ok(url.clone());
    }
    if let Some(path) = &cli.config {
        return load_config_url(path);
    }
    let mut candidates = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(format!("{home}/.config/trade-control/trade-control.toml"));
    }
    candidates.push("./trade-control.toml".to_string());
    for c in &candidates {
        if std::path::Path::new(c).is_file() {
            return load_config_url(c);
        }
    }
    Err(eyre!(
        "no database URL: pass --database-url, set DATABASE_URL, or create a \
         trade-control.toml (looked in ~/.config/trade-control/ and ./)"
    ))
}

fn load_config_url(path: &str) -> Result<String> {
    Ok(Config::load(path).map_err(|e| eyre!("{e}"))?.database.url)
}
