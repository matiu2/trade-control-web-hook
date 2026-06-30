//! `trade-control-accounts` — native account-index management.
//!
//! The Cloudflare `account` admin surface (`trade-control account list/add/…`,
//! talking to the worker's `/admin/accounts*` routes + `wrangler secret put`)
//! has no native equivalent yet: the VM worker serves only `POST /` + `GET
//! /health`. This CLI fills that gap by going **straight to Postgres** through
//! the same [`PgMetadataStore`] the worker uses, so list/add/remove behave
//! byte-for-byte like the dispatch path reads them — no HTTP, no admin route.
//!
//! It manages **metadata only** (name, broker, kind, caps, optional OANDA
//! sub-account id). Credentials are deliberately *not* here:
//!
//!   * **TradeNation** logins live in the enc store
//!     (`~/.config/tradenation/accounts.enc`, by name) — manage them with the
//!     TradeNation tooling (`create_account` / `delete_account`). `add` only
//!     records the metadata row; the account name must match an enc-store entry
//!     for a broker action to resolve.
//!   * **OANDA** uses the shared `OANDA_API_KEY` env token + the per-account
//!     `oanda_account_id` recorded here (an id, not a secret).
//!
//! DB connection: read from the worker's `trade-control.toml` (`--config`,
//! default `./trade-control.toml`) so it points at the same database as the
//! worker, or overridden with `--database-url`.
//!
//! ```text
//! trade-control-accounts list
//! trade-control-accounts add testing --broker tradenation --kind demo
//! trade-control-accounts add ms-oanda-1 --broker oanda --kind demo \
//!     --oanda-account-id 101-011-31142393-003 --max-risk-pct 0.5
//! trade-control-accounts remove testing
//! ```

use clap::{Parser, Subcommand, ValueEnum};
use color_eyre::eyre::{Context, Result, eyre};

use trade_control_core::account::{
    AccountCaps, AccountKind, AccountMetadata, MetadataError, MetadataStore,
};
use trade_control_core::intent::BrokerKind;
use trade_control_worker::{Config, PgMetadataStore, PgStateStore};

#[derive(Parser)]
#[command(name = "trade-control-accounts")]
#[command(about = "Manage the native (Postgres) account index — metadata only, no credentials")]
struct Cli {
    /// Path to the worker's `trade-control.toml`, used only to read
    /// `database.url`. Ignored when `--database-url` is given.
    #[arg(long, default_value = "./trade-control.toml", global = true)]
    config: String,

    /// Postgres URL override (`postgresql://…`). Takes precedence over the URL
    /// in `--config`, so the CLI can run without a config file present.
    #[arg(long, global = true)]
    database_url: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List every account in the index (name-ascending), metadata only.
    List,
    /// Show one account by name.
    Get {
        /// The account name.
        name: String,
    },
    /// Add a new account. Fails if the name already exists.
    Add {
        /// Stable account name — must match the enc-store entry (TradeNation)
        /// and the name the enter intent carries. `kebab-case`, unique.
        name: String,
        /// Which broker this account trades on.
        #[arg(long)]
        broker: BrokerArg,
        /// Demo or live.
        #[arg(long)]
        kind: KindArg,
        /// OANDA sub-account id (required for `--broker oanda`; ignored for
        /// TradeNation, where the session identifies the account).
        #[arg(long)]
        oanda_account_id: Option<String>,
        /// Optional per-account max risk % (tighter than the worker-wide cap).
        #[arg(long)]
        max_risk_pct: Option<f64>,
        /// Optional per-account max simultaneous open positions.
        #[arg(long)]
        max_open_positions: Option<u32>,
    },
    /// Remove an account by name. Fails if it doesn't exist.
    Remove {
        /// The account name to remove.
        name: String,
    },
}

/// CLI mirror of [`BrokerKind`] so clap can derive `--broker oanda|tradenation`.
#[derive(Clone, Copy, ValueEnum)]
enum BrokerArg {
    Oanda,
    Tradenation,
}

impl From<BrokerArg> for BrokerKind {
    fn from(b: BrokerArg) -> Self {
        match b {
            BrokerArg::Oanda => BrokerKind::Oanda,
            BrokerArg::Tradenation => BrokerKind::TradeNation,
        }
    }
}

/// CLI mirror of [`AccountKind`].
#[derive(Clone, Copy, ValueEnum)]
enum KindArg {
    Demo,
    Live,
}

impl From<KindArg> for AccountKind {
    fn from(k: KindArg) -> Self {
        match k {
            KindArg::Demo => AccountKind::Demo,
            KindArg::Live => AccountKind::Live,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    // Resolve the DB URL: explicit flag wins, else read it from the worker's
    // config so the CLI points at the same database the worker uses.
    let db_url = match cli.database_url {
        Some(url) => url,
        None => Config::load(&cli.config)
            .wrap_err_with(|| {
                format!(
                    "loading database.url from {} (pass --database-url to skip the config file)",
                    cli.config
                )
            })?
            .database
            .url,
    };

    let store = PgStateStore::connect(&db_url)
        .await
        .map_err(|e| eyre!("connecting to Postgres: {e}"))?;
    let accounts = PgMetadataStore::from_state_store(&store);

    match cli.command {
        Command::List => list(&accounts).await,
        Command::Get { name } => get(&accounts, &name).await,
        Command::Add {
            name,
            broker,
            kind,
            oanda_account_id,
            max_risk_pct,
            max_open_positions,
        } => {
            add(
                &accounts,
                name,
                broker.into(),
                kind.into(),
                oanda_account_id,
                max_risk_pct,
                max_open_positions,
            )
            .await
        }
        Command::Remove { name } => remove(&accounts, &name).await,
    }
}

/// Print the index as one line per account: `name  broker  kind  [oanda-id]  [caps]`.
async fn list(accounts: &PgMetadataStore) -> Result<()> {
    let rows = accounts.list().await.map_err(meta_err)?;
    if rows.is_empty() {
        println!("(no accounts)");
        return Ok(());
    }
    for m in &rows {
        println!("{}", render(m));
    }
    Ok(())
}

/// Print one account, or a clear not-found message.
async fn get(accounts: &PgMetadataStore, name: &str) -> Result<()> {
    match accounts.get(name).await {
        Ok(m) => {
            println!("{}", render(&m));
            Ok(())
        }
        Err(MetadataError::NotFound(_)) => Err(eyre!("no account named '{name}'")),
        Err(e) => Err(meta_err(e)),
    }
}

/// Add a new account row. OANDA requires a sub-account id (a broker action would
/// fail without it), so reject the obvious misconfiguration up front.
async fn add(
    accounts: &PgMetadataStore,
    name: String,
    broker: BrokerKind,
    kind: AccountKind,
    oanda_account_id: Option<String>,
    max_risk_pct: Option<f64>,
    max_open_positions: Option<u32>,
) -> Result<()> {
    if broker == BrokerKind::Oanda && oanda_account_id.is_none() {
        return Err(eyre!(
            "an OANDA account needs --oanda-account-id (the sub-account routed under the shared OANDA_API_KEY)"
        ));
    }
    let meta = AccountMetadata {
        name: name.clone(),
        broker,
        kind,
        caps: AccountCaps {
            max_risk_pct,
            max_open_positions,
        },
        oanda_account_id,
    };
    match accounts.add(meta.clone()).await {
        Ok(()) => {
            println!("added: {}", render(&meta));
            if broker == BrokerKind::TradeNation {
                println!(
                    "note: TradeNation credentials resolve from the enc store by name — \
                     ensure '{name}' exists there (TradeNation create_account)."
                );
            }
            Ok(())
        }
        Err(MetadataError::AlreadyExists(_)) => Err(eyre!(
            "account '{name}' already exists — remove it first, or pick another name"
        )),
        Err(e) => Err(meta_err(e)),
    }
}

/// Remove an account row by name.
async fn remove(accounts: &PgMetadataStore, name: &str) -> Result<()> {
    match accounts.remove(name).await {
        Ok(()) => {
            println!("removed: {name}");
            Ok(())
        }
        Err(MetadataError::NotFound(_)) => Err(eyre!("no account named '{name}'")),
        Err(e) => Err(meta_err(e)),
    }
}

/// One-line operator-facing rendering of an account's metadata.
fn render(m: &AccountMetadata) -> String {
    let broker = match m.broker {
        BrokerKind::Oanda => "oanda",
        BrokerKind::TradeNation => "tradenation",
    };
    let kind = match m.kind {
        AccountKind::Demo => "demo",
        AccountKind::Live => "live",
    };
    let mut s = format!("{:<16} {:<12} {kind}", m.name, broker);
    if let Some(id) = &m.oanda_account_id {
        s.push_str(&format!("  oanda_id={id}"));
    }
    if let Some(r) = m.caps.max_risk_pct {
        s.push_str(&format!("  max_risk_pct={r}"));
    }
    if let Some(p) = m.caps.max_open_positions {
        s.push_str(&format!("  max_open_positions={p}"));
    }
    s
}

/// Map a metadata-store backend error into an eyre report.
fn meta_err(e: MetadataError) -> color_eyre::eyre::Error {
    eyre!("account store error: {e}")
}
