//! CLI for the trade-control webhook.
//!
//! Subcommands:
//!   - `gen-key` — mint a fresh 32-byte signing key, print as hex on stdout.
//!   - `sign` — read an intent template (YAML), interactively prompt for
//!     any missing required fields, then emit the cleartext signed YAML
//!     alert body with TradingView `{{...}}` placeholders for the shell.
//!   - `verify` — recompute the HMAC over a signed body to confirm
//!     what arrived on the worker.
//!   - `status` — POST a signed control body to the deployed worker and
//!     print its YAML snapshot of cooldowns + recent seen ids.
//!   - `unlock <INSTRUMENT>` — POST a signed control body that clears
//!     the cooldown for one instrument.

use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use color_eyre::eyre::{Context, Result, eyre};
use trade_control_cli::{
    KEY_LEN, add_account, build_clear_prep_intent, build_clear_veto_intent, build_prep_intent,
    build_status_intent, build_unlock_intent, build_veto_intent, delete_account, delete_secret,
    fill_missing_fields, generate_key_hex, list_accounts, pick_template_interactive,
    prompt_save_as_template, put_secret, record_prep_use, record_veto_use, secret_binding_for,
    test_account, wrap_signed, wrap_signed_template,
};
use trade_control_core::account::{
    AccountKind, AccountMetadata, Credentials, OandaCreds, TradeNationCreds, TradeNationKind,
};
use trade_control_core::incoming::signed_pairs_from_text;
use trade_control_core::intent::Intent;
use trade_control_core::intent::{BrokerKind, VetoLevel};
use trade_control_core::sig::{self, SIG_FIELD};

#[derive(Parser)]
#[command(
    name = "trade-control",
    about = "Sign a trade intent for TradingView and manage worker state"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate a fresh 32-byte signing key as 64 hex characters.
    GenKey,
    /// Sign an intent YAML template into a cleartext YAML alert body.
    /// Intent fields are readable in TradingView and in Cloudflare's
    /// request log; authentication is HMAC-SHA256 over the body. Pair
    /// with `verify` to inspect what arrived on the worker.
    Sign(SignArgs),
    /// Query the deployed worker's cooldown / recent-seen state.
    Status(EndpointArgs),
    /// Clear the cooldown for one instrument on the deployed worker.
    Unlock(UnlockCmdArgs),
    /// Record a named prep step for an instrument with a TTL.
    Prep(PrepCmdArgs),
    /// Record a named veto for an instrument with a TTL.
    Veto(VetoCmdArgs),
    /// Clear a single prep flag.
    ClearPrep(ClearPrepCmdArgs),
    /// Clear a single veto flag.
    ClearVeto(ClearVetoCmdArgs),
    /// Verify a signed body. Reads the YAML body from a positional,
    /// `--file`, or stdin, recomputes the HMAC, and prints the body
    /// with a `# verified` marker on success. Exit code is non-zero
    /// on signature mismatch.
    Verify(VerifyArgs),
    /// Manage first-class accounts on the deployed worker. Talks to the
    /// `/admin/accounts*` routes; auth is `--admin-key-file` (distinct
    /// from `--key-file` used by intent endpoints). `account add` also
    /// wraps `wrangler secret put` for the credential half.
    #[command(subcommand)]
    Account(AccountCmd),
    /// Print a shell completion script to stdout. Install with e.g.
    /// `trade-control completions zsh > ~/.zfunc/_trade-control`.
    Completions {
        /// Target shell.
        shell: Shell,
    },
}

#[derive(Subcommand)]
enum AccountCmd {
    /// List configured accounts (metadata only, no credentials).
    List(AccountEndpointArgs),
    /// Add an account: writes metadata and (optionally) the credential
    /// secret via `wrangler secret put`. Prompts for any missing fields.
    Add(AccountAddArgs),
    /// Remove an account's metadata entry. Pass `--purge-secret` to also
    /// run `wrangler secret delete` for the credential binding.
    Delete(AccountDeleteArgs),
    /// Verify an account is wired up correctly: metadata exists, credential
    /// secret resolves, broker tags match. Does not log into the broker.
    Test(AccountTestArgs),
}

#[derive(Parser)]
struct AccountEndpointArgs {
    /// Path to a file containing the `ADMIN_KEY` secret (hex or plain
    /// text — read verbatim, trimmed). Distinct from `--key-file` so a
    /// leaked intent-auth key can't pivot to account mutation.
    #[arg(long, env = "TRADE_CONTROL_ADMIN_KEY_FILE")]
    admin_key_file: PathBuf,
    /// Worker URL (e.g. https://trade-control.<account>.workers.dev).
    /// Falls back to `TRADE_CONTROL_ENDPOINT`.
    #[arg(long, env = "TRADE_CONTROL_ENDPOINT")]
    endpoint: String,
}

#[derive(Parser)]
struct AccountAddArgs {
    /// Account name (kebab-case, unique within the worker).
    name: String,
    /// Broker the account belongs to.
    #[arg(long, value_enum)]
    broker: BrokerKindArg,
    /// Demo or live. Defaults to demo — live needs to be opted in
    /// deliberately.
    #[arg(long, value_enum, default_value_t = AccountKindArg::Demo)]
    kind: AccountKindArg,
    /// Maximum risk percent allowed against this account (tighter than
    /// the worker-wide cap; can only narrow, not relax).
    #[arg(long)]
    max_risk_pct: Option<f64>,
    /// Maximum simultaneous open positions for this account.
    #[arg(long)]
    max_open_positions: Option<u32>,
    /// Client-side minimum position size, in instrument units. Entries
    /// with `size_units` below this floor are rejected before reaching
    /// the broker. Only enforced when an intent uses the explicit
    /// `size_units` risk mode.
    #[arg(long)]
    min_position_size: Option<f64>,
    /// Skip the `wrangler secret put` step. Use when you've already set
    /// the credential secret manually, or you only want to write
    /// metadata first and provision credentials later.
    #[arg(long, default_value_t = false)]
    no_secret: bool,
    /// TradeNation: account username (the one that logs in). Prompted
    /// interactively if absent and the broker is TradeNation.
    #[arg(long)]
    username: Option<String>,
    /// OANDA: API key. Prompted interactively if absent and the broker
    /// is OANDA.
    #[arg(long)]
    api_key: Option<String>,
    /// OANDA: account id (e.g. `001-001-XXXX-001`). Prompted
    /// interactively if absent and the broker is OANDA.
    #[arg(long)]
    account_id: Option<String>,
    #[command(flatten)]
    common: AccountEndpointArgs,
}

#[derive(Parser)]
struct AccountDeleteArgs {
    /// Account name to remove.
    name: String,
    /// Also run `wrangler secret delete` for the credential binding.
    /// Off by default — deleting the metadata is reversible (just
    /// re-add); deleting the secret means re-entering credentials.
    #[arg(long, default_value_t = false)]
    purge_secret: bool,
    /// Broker, required only when `--purge-secret` is set so the CLI
    /// can compute the binding name without a round trip through the
    /// worker. Ignored otherwise.
    #[arg(long, value_enum)]
    broker: Option<BrokerKindArg>,
    #[command(flatten)]
    common: AccountEndpointArgs,
}

#[derive(Parser)]
struct AccountTestArgs {
    /// Account name to test.
    name: String,
    #[command(flatten)]
    common: AccountEndpointArgs,
}

/// Clap-side mirror of [`BrokerKind`]. Same pattern as
/// [`VetoLevelArg`] — keeps clap derive out of `core`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
enum BrokerKindArg {
    Oanda,
    TradeNation,
}

impl From<BrokerKindArg> for BrokerKind {
    fn from(v: BrokerKindArg) -> Self {
        match v {
            BrokerKindArg::Oanda => BrokerKind::Oanda,
            BrokerKindArg::TradeNation => BrokerKind::TradeNation,
        }
    }
}

/// Clap-side mirror of [`AccountKind`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
enum AccountKindArg {
    Demo,
    Live,
}

impl From<AccountKindArg> for AccountKind {
    fn from(v: AccountKindArg) -> Self {
        match v {
            AccountKindArg::Demo => AccountKind::Demo,
            AccountKindArg::Live => AccountKind::Live,
        }
    }
}

impl From<AccountKindArg> for TradeNationKind {
    fn from(v: AccountKindArg) -> Self {
        match v {
            AccountKindArg::Demo => TradeNationKind::Demo,
            AccountKindArg::Live => TradeNationKind::Live,
        }
    }
}

#[derive(Parser)]
struct EndpointArgs {
    /// Path to a hex-encoded 32-byte signing key.
    #[arg(long, env = "TRADE_CONTROL_KEY_FILE")]
    key_file: PathBuf,
    /// Worker URL (e.g. https://trade-control.<account>.workers.dev).
    /// Falls back to `TRADE_CONTROL_ENDPOINT`.
    #[arg(long, env = "TRADE_CONTROL_ENDPOINT")]
    endpoint: String,
}

#[derive(Parser)]
struct UnlockCmdArgs {
    /// Instrument to unlock, e.g. EUR_USD.
    instrument: String,
    #[command(flatten)]
    common: EndpointArgs,
}

#[derive(Parser)]
struct PrepCmdArgs {
    /// Instrument the prep applies to, e.g. EUR_USD.
    instrument: String,
    /// Named step that landed, e.g. break-and-close.
    step: String,
    /// TTL in hours before the prep auto-expires.
    #[arg(long, default_value_t = 4)]
    ttl_hours: u32,
    /// Comma-separated list of other prep steps to clear when this
    /// prep is recorded. Use to express ordered sequences — e.g.
    /// `--clears retest` on a `break-and-close` prep drops any stale
    /// `retest` so a future `requires_preps: [break-and-close, retest]`
    /// gate can't be satisfied by the pre-existing retest.
    #[arg(long, value_delimiter = ',', num_args = 0..)]
    clears: Vec<String>,
    #[command(flatten)]
    common: EndpointArgs,
}

#[derive(Parser)]
struct VetoCmdArgs {
    /// Instrument the veto applies to, e.g. EUR_USD.
    instrument: String,
    /// Named condition blocking entries, e.g. news-window.
    name: String,
    /// TTL in hours before the veto auto-expires.
    #[arg(long, default_value_t = 6)]
    ttl_hours: u32,
    /// Escalation level. `stop-next-entry` (default) only sets the KV
    /// flag. `cancel-pending` also cancels resting pending orders.
    /// `close-positions` also closes open positions.
    #[arg(long, value_enum, default_value_t = VetoLevelArg::StopNextEntry)]
    level: VetoLevelArg,
    /// Comma-separated list of other vetos to clear when this veto is
    /// recorded. Mirror of `prep --clears` for veto symmetry.
    #[arg(long, value_delimiter = ',', num_args = 0..)]
    clears: Vec<String>,
    #[command(flatten)]
    common: EndpointArgs,
}

/// Clap-side mirror of [`VetoLevel`]. Keeps clap derive out of `core`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
enum VetoLevelArg {
    StopNextEntry,
    CancelPending,
    ClosePositions,
}

impl From<VetoLevelArg> for VetoLevel {
    fn from(v: VetoLevelArg) -> Self {
        match v {
            VetoLevelArg::StopNextEntry => VetoLevel::StopNextEntry,
            VetoLevelArg::CancelPending => VetoLevel::CancelPending,
            VetoLevelArg::ClosePositions => VetoLevel::ClosePositions,
        }
    }
}

#[derive(Parser)]
struct ClearPrepCmdArgs {
    /// Instrument the prep applies to.
    instrument: String,
    /// Named step to clear.
    step: String,
    #[command(flatten)]
    common: EndpointArgs,
}

#[derive(Parser)]
struct ClearVetoCmdArgs {
    /// Instrument the veto applies to.
    instrument: String,
    /// Named veto to clear.
    name: String,
    #[command(flatten)]
    common: EndpointArgs,
}

#[derive(Parser)]
struct VerifyArgs {
    /// Path to a hex-encoded 32-byte key.
    #[arg(long, env = "TRADE_CONTROL_KEY_FILE")]
    key_file: PathBuf,
    /// Path to a file containing the full YAML alert body. If omitted
    /// and no positional `body` is given, stdin is read.
    #[arg(long)]
    file: Option<PathBuf>,
    /// The full YAML alert body, including the `sig:` line. Quote it.
    body: Option<String>,
}

#[derive(Parser)]
struct SignArgs {
    /// Path to a hex-encoded 32-byte signing key.
    #[arg(long, env = "TRADE_CONTROL_KEY_FILE")]
    key_file: PathBuf,
    /// Path to the intent template (YAML). If omitted, fuzzy-pick from
    /// `~/.config/trade-control/templates/**/*.yaml`. Missing required
    /// fields are prompted for unless `--non-interactive` is set.
    #[arg(long, alias = "input")]
    template: Option<PathBuf>,
    /// Hard-fail on any missing required field instead of prompting.
    #[arg(long, default_value_t = false)]
    non_interactive: bool,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::GenKey => {
            let hex_key = generate_key_hex();
            println!("{hex_key}");
        }
        Cmd::Sign(args) => run_sign(args)?,
        Cmd::Status(args) => run_status(args)?,
        Cmd::Unlock(args) => run_unlock(args)?,
        Cmd::Prep(args) => run_prep(args)?,
        Cmd::Veto(args) => run_veto(args)?,
        Cmd::ClearPrep(args) => run_clear_prep(args)?,
        Cmd::ClearVeto(args) => run_clear_veto(args)?,
        Cmd::Verify(args) => run_verify(args)?,
        Cmd::Account(sub) => run_account(sub)?,
        Cmd::Completions { shell } => run_completions(shell),
    }
    Ok(())
}

fn run_completions(shell: Shell) {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "trade-control", &mut io::stdout());
}

fn run_verify(args: VerifyArgs) -> Result<()> {
    let key = load_key(&args.key_file)?;
    let body = match (args.body, args.file) {
        (Some(b), _) => b,
        (None, Some(path)) => {
            fs::read_to_string(&path).with_context(|| format!("reading input {path:?}"))?
        }
        (None, None) => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .context("reading stdin")?;
            buf
        }
    };
    let pairs = signed_pairs_from_text(&body).map_err(|e| eyre!("parse pairs: {e}"))?;
    let sig_str = pairs
        .iter()
        .find(|(k, _)| k == SIG_FIELD)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| eyre!("body has no sig field — was it built with --signed?"))?;
    let without_sig: Vec<_> = pairs
        .iter()
        .filter(|(k, _)| k != SIG_FIELD)
        .cloned()
        .collect();
    sig::verify(&key, &without_sig, &sig_str).map_err(|e| eyre!("verify: {e}"))?;
    println!("# verified");
    print!("{body}");
    if !body.ends_with('\n') {
        println!();
    }
    Ok(())
}

fn load_key(path: &PathBuf) -> Result<[u8; KEY_LEN]> {
    let key_hex = fs::read_to_string(path).with_context(|| format!("reading key file {path:?}"))?;
    let key_bytes = hex::decode(key_hex.trim()).context("decoding hex key")?;
    key_bytes
        .try_into()
        .map_err(|_| eyre!("key must be exactly {KEY_LEN} bytes (64 hex chars)"))
}

/// Short random hex suffix for control envelope ids.
fn fresh_suffix() -> Result<String> {
    let mut bytes = [0u8; 2];
    getrandom::fill(&mut bytes).map_err(|e| eyre!("getrandom: {e}"))?;
    Ok(hex::encode(bytes))
}

/// Wrap a control intent in a signed body for the worker.
fn wrap_control(
    intent: &Intent,
    key: &[u8; KEY_LEN],
    now: chrono::DateTime<chrono::Utc>,
) -> Result<String> {
    wrap_signed(intent, key, now).map_err(|e| eyre!("wrap-signed: {e}"))
}

fn post_control(endpoint: &str, body: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| eyre!("http client: {e}"))?;
    let resp = client
        .post(endpoint)
        .header("content-type", "text/plain")
        .body(body.to_string())
        .send()
        .map_err(|e| eyre!("POST {endpoint}: {e}"))?;
    let status = resp.status();
    let text = resp.text().map_err(|e| eyre!("read response body: {e}"))?;
    if !status.is_success() {
        return Err(eyre!("worker returned {status}: {text}"));
    }
    Ok(text)
}

fn run_status(args: EndpointArgs) -> Result<()> {
    let key = load_key(&args.key_file)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_status_intent(now, &suffix);
    let body = wrap_control(&intent, &key, now)?;
    let response = post_control(&args.endpoint, &body)?;
    print!("{response}");
    Ok(())
}

fn run_unlock(args: UnlockCmdArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_unlock_intent(&args.instrument, now, &suffix);
    let body = wrap_control(&intent, &key, now)?;
    let response = post_control(&args.common.endpoint, &body)?;
    print!("{response}");
    Ok(())
}

fn run_prep(args: PrepCmdArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_prep_intent(
        &args.instrument,
        &args.step,
        args.ttl_hours,
        args.clears.clone(),
        now,
        &suffix,
    );
    let body = wrap_control(&intent, &key, now)?;
    let response = post_control(&args.common.endpoint, &body)?;
    record_prep_use(&args.step);
    // Also remember names from --clears so they suggest next time —
    // they're equally valid prep names by virtue of being used here.
    for c in &args.clears {
        record_prep_use(c);
    }
    print!("{response}");
    Ok(())
}

fn run_veto(args: VetoCmdArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    // Default level is sent as `None` to keep the wire form minimal —
    // the worker treats absent and `stop-next-entry` identically.
    let level: Option<VetoLevel> = match args.level {
        VetoLevelArg::StopNextEntry => None,
        other => Some(other.into()),
    };
    let intent = build_veto_intent(
        &args.instrument,
        &args.name,
        args.ttl_hours,
        level,
        args.clears.clone(),
        now,
        &suffix,
    );
    let body = wrap_control(&intent, &key, now)?;
    let response = post_control(&args.common.endpoint, &body)?;
    record_veto_use(&args.name);
    for c in &args.clears {
        record_veto_use(c);
    }
    print!("{response}");
    Ok(())
}

fn run_clear_prep(args: ClearPrepCmdArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_clear_prep_intent(&args.instrument, &args.step, now, &suffix);
    let body = wrap_control(&intent, &key, now)?;
    let response = post_control(&args.common.endpoint, &body)?;
    print!("{response}");
    Ok(())
}

fn run_clear_veto(args: ClearVetoCmdArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_clear_veto_intent(&args.instrument, &args.name, now, &suffix);
    let body = wrap_control(&intent, &key, now)?;
    let response = post_control(&args.common.endpoint, &body)?;
    print!("{response}");
    Ok(())
}

/// Read an intent template, fill in any missing required fields
/// (interactively unless `--non-interactive`), then emit the cleartext
/// signed YAML alert body with TradingView `{{...}}` placeholders for
/// the shell.
fn run_sign(args: SignArgs) -> Result<()> {
    let key = load_key(&args.key_file)?;

    let template_path = match args.template {
        Some(p) => p,
        None => {
            if args.non_interactive {
                return Err(eyre!(
                    "--template is required when --non-interactive is set"
                ));
            }
            pick_template_interactive()?
        }
    };

    let template_str = fs::read_to_string(&template_path)
        .with_context(|| format!("reading template {template_path:?}"))?;
    let mut template: serde_yaml::Value =
        serde_yaml::from_str(&template_str).context("template is not valid YAML")?;
    if !template.is_mapping() {
        return Err(eyre!("template root must be a YAML mapping"));
    }

    fill_missing_fields(&mut template, args.non_interactive)?;

    let completed = serde_yaml::to_string(&template).context("re-serialising completed intent")?;

    // Offer to save the completed (post-prompt) YAML to disk so the user
    // can use it as a starting point next time. Done *before* the body
    // prints so the prompt doesn't interleave with the paste target.
    if !args.non_interactive {
        prompt_save_as_template(&completed)?;
    }

    // Deserialise into a typed Intent so wrap_signed_template serialises
    // it back through the standard path. This also catches
    // intent-validation issues before the user pastes the body.
    let intent: Intent =
        serde_yaml::from_str(&completed).context("completed intent does not parse")?;
    let body = wrap_signed_template(&intent, &key)?;
    print!("{body}");
    Ok(())
}

/// Load the admin-auth key from disk. Trims trailing whitespace so a
/// `wrangler secret put` round-trip (which echoes back the value) can
/// be saved straight to disk without surprises. Empty file is an error.
fn load_admin_key(path: &PathBuf) -> Result<String> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading admin key {path:?}"))?;
    let trimmed = raw.trim().to_owned();
    if trimmed.is_empty() {
        return Err(eyre!("admin key file is empty"));
    }
    Ok(trimmed)
}

fn run_account(sub: AccountCmd) -> Result<()> {
    match sub {
        AccountCmd::List(args) => run_account_list(args),
        AccountCmd::Add(args) => run_account_add(args),
        AccountCmd::Delete(args) => run_account_delete(args),
        AccountCmd::Test(args) => run_account_test(args),
    }
}

fn run_account_list(args: AccountEndpointArgs) -> Result<()> {
    let admin_key = load_admin_key(&args.admin_key_file)?;
    let body = list_accounts(&args.endpoint, &admin_key)?;
    print!("{body}");
    if !body.ends_with('\n') {
        println!();
    }
    Ok(())
}

fn run_account_test(args: AccountTestArgs) -> Result<()> {
    let admin_key = load_admin_key(&args.common.admin_key_file)?;
    let body = test_account(&args.common.endpoint, &admin_key, &args.name)?;
    print!("{body}");
    if !body.ends_with('\n') {
        println!();
    }
    Ok(())
}

fn run_account_delete(args: AccountDeleteArgs) -> Result<()> {
    let admin_key = load_admin_key(&args.common.admin_key_file)?;
    if args.purge_secret {
        // Compute the binding name before contacting the worker — if
        // the broker arg is missing we want to fail fast rather than
        // after the metadata is already gone.
        let broker: BrokerKind = args
            .broker
            .ok_or_else(|| eyre!("--purge-secret requires --broker"))?
            .into();
        let binding = secret_binding_for(broker, &args.name);
        let body = delete_account(&args.common.endpoint, &admin_key, &args.name)?;
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        eprintln!("purging credential secret {binding}…");
        delete_secret(&binding)?;
    } else {
        let body = delete_account(&args.common.endpoint, &admin_key, &args.name)?;
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

fn run_account_add(args: AccountAddArgs) -> Result<()> {
    use dialoguer::{Input, Password, theme::ColorfulTheme};

    let admin_key = load_admin_key(&args.common.admin_key_file)?;
    let broker: BrokerKind = args.broker.into();
    let kind: AccountKind = args.kind.into();

    // Caps — only attach if either field was supplied; otherwise default
    // skip-serialise keeps the wire form minimal.
    let caps = trade_control_core::account::AccountCaps {
        max_risk_pct: args.max_risk_pct,
        max_open_positions: args.max_open_positions,
        min_position_size: args.min_position_size,
    };
    let metadata = AccountMetadata {
        name: args.name.clone(),
        broker,
        kind,
        caps,
    };

    // Build credentials *first* so we fail before touching the worker
    // if the operator aborts at the prompt. The credential half is the
    // expensive thing to redo.
    let creds = if args.no_secret {
        None
    } else {
        let theme = ColorfulTheme::default();
        match broker {
            BrokerKind::TradeNation => {
                let username = match args.username {
                    Some(u) => u,
                    None => Input::with_theme(&theme)
                        .with_prompt("TradeNation username")
                        .interact_text()
                        .map_err(|e| eyre!("read username: {e}"))?,
                };
                let password = Password::with_theme(&theme)
                    .with_prompt("TradeNation password")
                    .interact()
                    .map_err(|e| eyre!("read password: {e}"))?;
                Some(Credentials::TradeNation(TradeNationCreds {
                    kind: args.kind.into(),
                    username,
                    password,
                }))
            }
            BrokerKind::Oanda => {
                let api_key = match args.api_key {
                    Some(k) => k,
                    None => Password::with_theme(&theme)
                        .with_prompt("OANDA API key")
                        .interact()
                        .map_err(|e| eyre!("read api key: {e}"))?,
                };
                let account_id = match args.account_id {
                    Some(id) => id,
                    None => Input::with_theme(&theme)
                        .with_prompt("OANDA account id")
                        .interact_text()
                        .map_err(|e| eyre!("read account id: {e}"))?,
                };
                Some(Credentials::Oanda(OandaCreds {
                    api_key,
                    account_id,
                }))
            }
        }
    };

    // Write metadata first. If this fails (e.g. already-exists) we
    // haven't touched secrets — the operator can re-run.
    let add_body = add_account(&args.common.endpoint, &admin_key, &metadata)?;
    print!("{add_body}");
    if !add_body.ends_with('\n') {
        println!();
    }

    // Then push the credential to Secret Store. The two-step shape is
    // deliberate — a failure here leaves the operator with a metadata
    // entry pointing at a missing secret, which `account test` will
    // surface as a 424. They can then re-run with `--no-secret` skipped
    // to retry the secret-put alone.
    if let Some(creds) = creds {
        let binding = secret_binding_for(broker, &args.name);
        eprintln!("uploading credential secret {binding} via wrangler…");
        put_secret(&binding, &creds)?;
    } else {
        eprintln!(
            "skipped credential secret (use `wrangler secret put {}` to set it)",
            secret_binding_for(broker, &args.name)
        );
    }

    Ok(())
}
