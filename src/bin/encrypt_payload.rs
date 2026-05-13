//! CLI that turns a YAML trade intent (possibly with missing fields) into
//! the YAML body for a TradingView alert template, with the intent
//! encrypted under a shared key.
//!
//! Two subcommands:
//!   - `gen-key` — mint a fresh 32-byte key, print as hex on stdout.
//!   - `encrypt` — read an intent template (YAML), interactively prompt for
//!     any missing required fields, then emit the YAML alert body with
//!     TradingView `{{...}}` placeholders for the plaintext shell.

use std::fs;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use color_eyre::eyre::{Context, Result, eyre};
use trade_control_web_hook::cli::{
    KEY_LEN, build_yaml_template, encrypt_intent, fill_missing_fields, generate_key_hex,
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
    }
    Ok(())
}

fn run_encrypt(args: EncryptArgs) -> Result<()> {
    let key_hex = fs::read_to_string(&args.key_file)
        .with_context(|| format!("reading key file {:?}", args.key_file))?;
    let key_bytes = hex::decode(key_hex.trim()).context("decoding hex key")?;
    let key: [u8; KEY_LEN] = key_bytes
        .try_into()
        .map_err(|_| eyre!("key must be exactly {KEY_LEN} bytes (64 hex chars)"))?;

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
