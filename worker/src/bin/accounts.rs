//! `trade-control-accounts` — native account-index management.
//!
//! The Cloudflare `account` admin surface (`trade-control account list/add/…`,
//! talking to the worker's `/admin/accounts*` routes + `wrangler secret put`)
//! has no native equivalent yet: the VM worker serves only `POST /` + `GET
//! /health`. This CLI fills that gap by going **straight to Postgres** through
//! the same [`PgMetadataStore`] the worker uses, so list/add/remove behave
//! byte-for-byte like the dispatch path reads them — no HTTP, no admin route.
//!
//! Metadata lives in Postgres. MT5 `add` and `credentials` also maintain the
//! named local profile and its private credential file. TradeNation credentials
//! remain in the enc store; OANDA uses OANDA_API_KEY and its sub-account id.
//!
//! DB connection: `--database-url` / `DATABASE_URL` env win; otherwise the URL
//! is read from a `trade-control.toml` — an explicit `--config <path>`, else
//! `~/.config/trade-control/trade-control.toml` (the config home, searched
//! first so the CLI works from any directory), else `./trade-control.toml`. The
//! account *data* itself lives in Postgres (the `accounts` table), so it's
//! globally accessible to anything pointed at the same database.
//!
//! ```text
//! trade-control-accounts list
//! trade-control-accounts add testing --broker tradenation --kind demo
//! trade-control-accounts add ms-oanda-1 --broker oanda --kind demo \
//!     --oanda-account-id 101-011-31142393-003 --max-risk-pct 0.5
//! trade-control-accounts remove testing
//! ```

use clap::{Args, Parser, Subcommand, ValueEnum};
use color_eyre::eyre::{Context, Result, eyre};

use trade_control_core::account::{
    AccountCaps, AccountKind, AccountMetadata, MetadataError, MetadataStore,
};
use trade_control_core::intent::BrokerKind;
use trade_control_worker::{Config, PgMetadataStore, PgStateStore};

#[path = "accounts/mt5.rs"]
mod mt5;
#[path = "accounts/registration.rs"]
mod registration;

#[derive(Parser)]
#[command(name = "trade-control-accounts")]
#[command(about = "Manage the native account index and local MT5 credentials")]
struct Cli {
    /// Path to the worker's `trade-control.toml` to read `database.url` from.
    /// When omitted, the CLI resolves the DB URL via (in order): `--database-url`,
    /// the `DATABASE_URL` env var, `~/.config/trade-control/trade-control.toml`,
    /// then `./trade-control.toml`. Ignored when
    /// `--database-url` is given.
    #[arg(long, global = true)]
    config: Option<String>,

    /// Postgres URL override (`postgresql://…`). Highest precedence — lets the
    /// CLI run with no config file present.
    #[arg(long, env = "DATABASE_URL", global = true)]
    database_url: Option<String>,

    #[command(subcommand)]
    command: Command,
}

/// Resolve the Postgres URL with a forgiving precedence chain so the CLI is
/// usable from any directory:
///
/// 1. `--database-url` / `DATABASE_URL` env (clap already merged these).
/// 2. an explicit `--config <path>` (a missing file here IS an error — the
///    operator named it).
/// 3. `~/.config/trade-control/trade-control.toml` if it exists.
/// 4. `./trade-control.toml` if it exists (the worker's working-dir convention).
///
/// If none resolve, the error lists every option so the fix is obvious.
fn resolve_db_url(cli: &Cli) -> Result<String> {
    if let Some(url) = &cli.database_url {
        return Ok(url.clone());
    }
    if let Some(path) = &cli.config {
        return load_config_url(path)
            .wrap_err_with(|| format!("loading database.url from --config {path}"));
    }
    for candidate in default_config_paths() {
        if std::path::Path::new(&candidate).is_file() {
            return load_config_url(&candidate)
                .wrap_err_with(|| format!("loading database.url from {candidate}"));
        }
    }
    Err(eyre!(
        "no database URL: pass --database-url, set DATABASE_URL, or create a \
         trade-control.toml (looked in ./ and ~/.config/trade-control/) — or \
         point --config at one"
    ))
}

/// Read just `database.url` out of a config TOML.
fn load_config_url(path: &str) -> Result<String> {
    Ok(Config::load(path).map_err(|e| eyre!("{e}"))?.database.url)
}

/// The default config locations searched when neither `--database-url` nor
/// `--config` is given. The user config dir (`~/.config/trade-control/`, the
/// established home for `key.hex` / `admin-key.hex` / templates) is searched
/// **first** so the CLI works from any directory and isn't shadowed by a stray
/// `./trade-control.toml`; the working-dir file (the worker's own convention)
/// is the fallback.
fn default_config_paths() -> Vec<String> {
    let mut paths = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        paths.push(format!("{home}/.config/trade-control/trade-control.toml"));
    }
    paths.push("./trade-control.toml".to_string());
    paths
}

#[derive(Subcommand)]
enum Command {
    /// List every account in the index (name-ascending), metadata only.
    List {
        /// Emit non-secret account metadata as a JSON array.
        #[arg(long)]
        json: bool,
    },
    /// Show one account by name.
    Get {
        /// The account name.
        name: String,
    },
    /// Add a new account. MT5 also writes its profile and prompts for a password.
    Add(AddArgs),
    /// Replace an existing MT5 account's credentials and pinned identity.
    Credentials {
        name: String,
        #[command(flatten)]
        mt5: mt5::Options,
    },
    /// Remove an account by name. Fails if it doesn't exist.
    Remove {
        /// The account name to remove.
        name: String,
    },
}

#[derive(Args)]
struct AddArgs {
    /// Stable account name — must match the enc-store entry (TradeNation)
    /// and the name the enter intent carries. `kebab-case`, unique.
    name: String,
    /// Which broker this account trades on.
    #[arg(long)]
    broker: BrokerArg,
    /// Demo or live.
    #[arg(long)]
    kind: KindArg,
    /// Broker sub-account id. **Required** for `--broker oanda` (the
    /// sub-account under the shared API key) and for `--broker ibkr` (the
    /// account under the Gateway login, e.g. `DUR300718` for paper).
    /// Ignored for TradeNation, where the session identifies the account.
    #[arg(long)]
    oanda_account_id: Option<String>,
    /// Optional per-account max risk % (tighter than the worker-wide cap).
    #[arg(long)]
    max_risk_pct: Option<f64>,
    /// Optional per-account max simultaneous open positions.
    #[arg(long)]
    max_open_positions: Option<u32>,
    #[command(flatten)]
    mt5: mt5::Options,
}

/// CLI mirror of [`BrokerKind`] so clap can derive
/// `--broker oanda|tradenation|ibkr`.
#[derive(Clone, Copy, ValueEnum)]
enum BrokerArg {
    Oanda,
    Tradenation,
    Ibkr,
    Mt5,
    Ctrader,
}

impl From<BrokerArg> for BrokerKind {
    fn from(b: BrokerArg) -> Self {
        match b {
            BrokerArg::Oanda => BrokerKind::Oanda,
            BrokerArg::Tradenation => BrokerKind::TradeNation,
            BrokerArg::Ibkr => BrokerKind::Ibkr,
            BrokerArg::Mt5 => BrokerKind::Mt5,
            BrokerArg::Ctrader => BrokerKind::Ctrader,
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
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .try_init()
        .ok();
    let cli = Cli::parse();

    let db_url = resolve_db_url(&cli)?;

    let store = PgStateStore::connect(&db_url)
        .await
        .map_err(|e| eyre!("connecting to Postgres: {e}"))?;
    let accounts = PgMetadataStore::from_state_store(&store);

    match cli.command {
        Command::List { json } => list(&accounts, json).await,
        Command::Get { name } => get(&accounts, &name).await,
        Command::Add(args) => add(&accounts, args).await,
        Command::Credentials { name, mt5 } => {
            let path = mt5::config_path()?;
            registration::credentials(&accounts, &name, &mt5, &path, || mt5.password()).await?;
            println!("updated MT5 credentials: {name}");
            print_mt5_restart();
            Ok(())
        }
        Command::Remove { name } => remove(&accounts, &name).await,
    }
}

/// Print the index as one line per account: `name  broker  kind  [oanda-id]  [caps]`.
async fn list(accounts: &PgMetadataStore, json: bool) -> Result<()> {
    let rows = accounts.list().await.map_err(meta_err)?;
    if json {
        let rows = rows
            .iter()
            .map(trade_control_worker::account_capabilities::listing)
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string(&rows)?);
        return Ok(());
    }
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
async fn add(accounts: &impl MetadataStore, args: AddArgs) -> Result<()> {
    let AddArgs {
        name,
        broker,
        kind,
        oanda_account_id,
        max_risk_pct,
        max_open_positions,
        mt5,
    } = args;
    let broker: BrokerKind = broker.into();
    let kind: AccountKind = kind.into();
    if broker != BrokerKind::Mt5 && mt5.is_present() {
        return Err(eyre!("MT5 credential options require --broker mt5"));
    }
    if broker == BrokerKind::Ctrader {
        broker_ctrader::access::Profile::load(&name)?
            .validate(kind.is_live(), oanda_account_id.as_deref())?;
    }
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
    let path = if broker == BrokerKind::Mt5 {
        Some(mt5::config_path()?)
    } else {
        None
    };
    registration::add(accounts, meta.clone(), &mt5, path.as_deref(), || {
        mt5.password()
    })
    .await?;
    println!("added: {}", render(&meta));
    if broker == BrokerKind::TradeNation {
        println!(
            "note: TradeNation credentials resolve from the enc store by name — ensure '{name}' exists there (TradeNation create_account)."
        );
    }
    if broker == BrokerKind::Mt5 {
        print_mt5_restart();
    }
    Ok(())
}

fn print_mt5_restart() {
    println!(
        "MT5 profile and private credentials saved locally. Restart local-chart and any workers using this account to load the changes. Log the desktop MT5 terminal into the same account for CandleBridge history."
    );
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
    let broker = m.broker.as_str();
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
    if m.broker == BrokerKind::Ctrader {
        let capabilities = trade_control_worker::account_capabilities::capabilities(m);
        s.push_str(&format!(
            "  {} ({})",
            capabilities.readiness, capabilities.reason
        ));
    }
    s
}

/// Map a metadata-store backend error into an eyre report.
fn meta_err(e: MetadataError) -> color_eyre::eyre::Error {
    eyre!("account store error: {e}")
}

#[cfg(test)]
#[path = "accounts/tests.rs"]
mod tests;
