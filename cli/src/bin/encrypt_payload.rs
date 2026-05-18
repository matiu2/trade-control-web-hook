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
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use color_eyre::eyre::{Context, Result, eyre};
use trade_control_cli::{
    KEY_LEN, build_clear_prep_intent, build_clear_veto_intent, build_prep_intent,
    build_status_intent, build_unlock_intent, build_veto_intent, build_yaml_template,
    encrypt_intent, fill_missing_fields, generate_key_hex, pick_template_interactive,
    prompt_save_as_template, record_prep_use, record_veto_use, wrap_in_envelope, wrap_signed,
    wrap_signed_template,
};
use trade_control_core::crypto;
use trade_control_core::incoming::signed_pairs_from_text;
use trade_control_core::intent::Intent;
use trade_control_core::intent::VetoLevel;
use trade_control_core::sig::{self, SIG_FIELD};

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
    /// The body is opaque on the wire; debugging requires `decrypt`.
    Encrypt(EncryptArgs),
    /// Sign an intent YAML template into a *cleartext* YAML alert body.
    /// Intent fields are readable in TradingView and in Cloudflare's
    /// request log; authentication is HMAC-SHA256 over the body. Pair
    /// with `verify` to inspect what arrived on the worker.
    Sign(EncryptArgs),
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
    /// Decrypt a `v1.<base64>` payload back to the plaintext intent YAML.
    /// Accepts either the bare `v1.…` blob as a positional, the full
    /// YAML alert body on stdin, or a `--file` path. Useful for inspecting
    /// what TradingView actually sent.
    Decrypt(DecryptArgs),
    /// Verify a signed (cleartext) body. Reads the YAML body from a
    /// positional, `--file`, or stdin, recomputes the HMAC, and prints
    /// the body with a `# verified` marker on success. Exit code is
    /// non-zero on signature mismatch.
    Verify(VerifyArgs),
    /// Print a shell completion script to stdout. Install with e.g.
    /// `encrypt-payload completions zsh > ~/.zfunc/_encrypt-payload`.
    Completions {
        /// Target shell.
        shell: Shell,
    },
}

#[derive(Parser)]
struct EndpointArgs {
    /// Path to a hex-encoded 32-byte key.
    #[arg(long, env = "TRADE_CONTROL_KEY_FILE")]
    key_file: PathBuf,
    /// Worker URL (e.g. https://trade-control.<account>.workers.dev).
    /// Falls back to `TRADE_CONTROL_ENDPOINT`.
    #[arg(long, env = "TRADE_CONTROL_ENDPOINT")]
    endpoint: String,
    /// Use the signed (cleartext) wire format instead of encrypted.
    /// Cleartext means the intent shows up in Cloudflare's request log
    /// — easier debugging at the cost of revealing the intent body.
    /// Authentication is unchanged (HMAC-SHA256 over the body).
    #[arg(long, default_value_t = false)]
    signed: bool,
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
struct DecryptArgs {
    /// Path to a hex-encoded 32-byte key.
    #[arg(long, env = "TRADE_CONTROL_KEY_FILE")]
    key_file: PathBuf,
    /// Path to a file containing either the bare `v1.…` blob or the
    /// full YAML alert body. If omitted and no positional `blob` is
    /// given, stdin is read.
    #[arg(long)]
    file: Option<PathBuf>,
    /// The encrypted payload as a `v1.<base64>` blob, OR the full YAML
    /// alert body containing a `payload: "v1.…"` line. Quote it.
    blob: Option<String>,
}

#[derive(Parser)]
struct EncryptArgs {
    /// Path to a hex-encoded 32-byte key.
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
        Cmd::Encrypt(args) => run_encrypt_or_sign(args, false)?,
        Cmd::Sign(args) => run_encrypt_or_sign(args, true)?,
        Cmd::Status(args) => run_status(args)?,
        Cmd::Unlock(args) => run_unlock(args)?,
        Cmd::Prep(args) => run_prep(args)?,
        Cmd::Veto(args) => run_veto(args)?,
        Cmd::ClearPrep(args) => run_clear_prep(args)?,
        Cmd::ClearVeto(args) => run_clear_veto(args)?,
        Cmd::Decrypt(args) => run_decrypt(args)?,
        Cmd::Verify(args) => run_verify(args)?,
        Cmd::Completions { shell } => run_completions(shell),
    }
    Ok(())
}

fn run_completions(shell: Shell) {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "encrypt-payload", &mut io::stdout());
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

fn run_decrypt(args: DecryptArgs) -> Result<()> {
    let key = load_key(&args.key_file)?;
    let raw = match (args.blob, args.file) {
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
    let blob = extract_payload_blob(&raw)?;
    let plain = crypto::decrypt(&key, &blob).map_err(|e| eyre!("decrypt: {e}"))?;
    let text = String::from_utf8(plain).context("plaintext is not valid UTF-8")?;
    print!("{text}");
    if !text.ends_with('\n') {
        println!();
    }
    Ok(())
}

/// Pull a `v1.<base64>` blob out of either a bare string or a YAML body
/// with a `payload: "v1.…"` field. Trims whitespace and surrounding quotes.
fn extract_payload_blob(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.starts_with("v1.") {
        return Ok(strip_quotes(trimmed).to_string());
    }
    // Try YAML — pull the `payload` field.
    if let Ok(serde_yaml::Value::Mapping(map)) = serde_yaml::from_str::<serde_yaml::Value>(trimmed)
        && let Some(v) = map.get(serde_yaml::Value::String("payload".to_string()))
        && let Some(s) = v.as_str()
        && s.starts_with("v1.")
    {
        return Ok(s.to_string());
    }
    // Fallback: scan line-by-line for `payload:` so a body with TradingView
    // `{{close}}` placeholders (which aren't valid YAML scalars) still works.
    for line in trimmed.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("payload:") {
            let val = strip_quotes(rest.trim());
            if val.starts_with("v1.") {
                return Ok(val.to_string());
            }
        }
    }
    Err(eyre!(
        "no `v1.<base64>` payload found in input; pass the blob as a positional, --file, or pipe the YAML body in on stdin"
    ))
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(s)
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

/// Wrap a control intent in either the encrypted or signed envelope
/// depending on the `--signed` flag.
fn wrap_control(
    intent: &Intent,
    key: &[u8; KEY_LEN],
    now: chrono::DateTime<chrono::Utc>,
    signed: bool,
) -> Result<String> {
    if signed {
        wrap_signed(intent, key, now).map_err(|e| eyre!("wrap-signed: {e}"))
    } else {
        wrap_in_envelope(intent, key, now).map_err(|e| eyre!("wrap-envelope: {e}"))
    }
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
    let body = wrap_control(&intent, &key, now, args.signed)?;
    let response = post_control(&args.endpoint, &body)?;
    print!("{response}");
    Ok(())
}

fn run_unlock(args: UnlockCmdArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_unlock_intent(&args.instrument, now, &suffix);
    let body = wrap_control(&intent, &key, now, args.common.signed)?;
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
    let body = wrap_control(&intent, &key, now, args.common.signed)?;
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
    let body = wrap_control(&intent, &key, now, args.common.signed)?;
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
    let body = wrap_control(&intent, &key, now, args.common.signed)?;
    let response = post_control(&args.common.endpoint, &body)?;
    print!("{response}");
    Ok(())
}

fn run_clear_veto(args: ClearVetoCmdArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_clear_veto_intent(&args.instrument, &args.name, now, &suffix);
    let body = wrap_control(&intent, &key, now, args.common.signed)?;
    let response = post_control(&args.common.endpoint, &body)?;
    print!("{response}");
    Ok(())
}

/// Shared body of the `encrypt` and `sign` subcommands. `sign = true`
/// emits the cleartext signed wire format; otherwise the encrypted blob.
fn run_encrypt_or_sign(args: EncryptArgs, sign_mode: bool) -> Result<()> {
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

    if sign_mode {
        // Deserialise into a typed Intent so wrap_signed_template can
        // serialise it back through the standard path. This also catches
        // intent-validation issues before the user pastes the body.
        let intent: Intent =
            serde_yaml::from_str(&completed).context("completed intent does not parse")?;
        let body = wrap_signed_template(&intent, &key)?;
        print!("{body}");
    } else {
        let blob = encrypt_intent(&key, completed.as_bytes())?;
        let yaml = build_yaml_template(&blob);
        print!("{yaml}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOB: &str = "v1.iiLvZCndSf+I3toNPFOtMDQixC2eMYO2TovfKKgepdIW";

    #[test]
    fn extract_payload_from_bare_blob() {
        assert_eq!(extract_payload_blob(BLOB).unwrap(), BLOB);
    }

    #[test]
    fn extract_payload_from_bare_blob_with_whitespace() {
        let padded = format!("  \n{BLOB}\n  ");
        assert_eq!(extract_payload_blob(&padded).unwrap(), BLOB);
    }

    #[test]
    fn extract_payload_from_yaml_body() {
        let yaml = format!(
            "close: 1.23\nhigh: 1.24\nlow: 1.22\ntime: \"2026-05-18T00:00:00Z\"\npayload: \"{BLOB}\"\n"
        );
        assert_eq!(extract_payload_blob(&yaml).unwrap(), BLOB);
    }

    #[test]
    fn extract_payload_from_tradingview_template_with_placeholders() {
        // Body still has `{{close}}` placeholders — not valid YAML — but
        // the line-scan fallback should still pull payload out.
        let yaml = format!(
            "close: {{{{close}}}}\nhigh: {{{{high}}}}\nlow: {{{{low}}}}\ntime: \"{{{{time}}}}\"\npayload: \"{BLOB}\"\n"
        );
        assert_eq!(extract_payload_blob(&yaml).unwrap(), BLOB);
    }

    #[test]
    fn extract_payload_rejects_missing_payload() {
        let yaml = "close: 1\nhigh: 2\nlow: 0\ntime: \"now\"\n";
        assert!(extract_payload_blob(yaml).is_err());
    }
}
