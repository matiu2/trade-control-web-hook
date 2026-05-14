//! CLI for the trade-control webhook.
//!
//! Subcommands:
//!   - `gen-key` — mint a fresh 32-byte key, print as hex on stdout.
//!   - `encrypt` — read an intent template (YAML), interactively prompt for
//!     any missing required fields, then emit the YAML alert body with
//!     TradingView `{{...}}` placeholders for the plaintext shell.
//!   - `status` — POST a control envelope to the deployed worker and print
//!     its YAML snapshot of cooldowns + recent seen ids.
//!   - `unlock <INSTRUMENT>` — POST a control envelope that clears the
//!     cooldown for one instrument.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use clap::{Parser, Subcommand};
use color_eyre::eyre::{Context, Result, eyre};
use trade_control_cli::{
    KEY_LEN, build_status_intent, build_unlock_intent, build_yaml_template, encrypt_intent,
    fill_missing_fields, generate_key_hex, wrap_in_envelope,
};

#[derive(Parser)]
#[command(
    name = "encrypt-payload",
    about = "Encrypt a trade intent for TradingView"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate a fresh 32-byte key as 64 hex characters.
    GenKey,
    /// Encrypt an intent YAML template into the YAML alert body.
    Encrypt(EncryptArgs),
    /// Query the deployed worker's cooldown / recent-seen state.
    Status(EndpointArgs),
    /// Clear the cooldown for one instrument on the deployed worker.
    Unlock(UnlockCmdArgs),
}

#[derive(Parser)]
struct EndpointArgs {
    /// Path to a hex-encoded 32-byte key.
    #[arg(long)]
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
struct EncryptArgs {
    /// Path to a hex-encoded 32-byte key.
    #[arg(long)]
    key_file: PathBuf,
    /// Path to the intent template (YAML). Missing required fields are
    /// prompted for unless `--non-interactive` is set.
    #[arg(long, alias = "input")]
    template: PathBuf,
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
        Cmd::Encrypt(args) => run_encrypt(args)?,
        Cmd::Status(args) => run_status(args)?,
        Cmd::Unlock(args) => run_unlock(args)?,
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
    let body = wrap_in_envelope(&intent, &key, now)?;
    let response = post_control(&args.endpoint, &body)?;
    print!("{response}");
    Ok(())
}

fn run_unlock(args: UnlockCmdArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_unlock_intent(&args.instrument, now, &suffix);
    let body = wrap_in_envelope(&intent, &key, now)?;
    let response = post_control(&args.common.endpoint, &body)?;
    print!("{response}");
    Ok(())
}

fn run_encrypt(args: EncryptArgs) -> Result<()> {
    let key = load_key(&args.key_file)?;

    let template_str = fs::read_to_string(&args.template)
        .with_context(|| format!("reading template {:?}", args.template))?;
    let mut template: serde_yaml::Value =
        serde_yaml::from_str(&template_str).context("template is not valid YAML")?;
    if !template.is_mapping() {
        return Err(eyre!("template root must be a YAML mapping"));
    }

    fill_missing_fields(&mut template, args.non_interactive)?;

    let completed = serde_yaml::to_string(&template).context("re-serialising completed intent")?;
    let blob = encrypt_intent(&key, completed.as_bytes())?;
    let yaml = build_yaml_template(&blob);
    print!("{yaml}");
    Ok(())
}
