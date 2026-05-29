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
    CalendarBarsArgs, KEY_LEN, TradePattern, add_account, build_clear_prep_intent,
    build_clear_veto_intent, build_news_from_spec, build_pause_from_spec, build_prep_intent,
    build_status_intent, build_trade_from_spec, build_trade_interactive, build_unlock_intent,
    build_veto_intent, delete_account, delete_secret, fill_missing_fields, generate_key_hex,
    list_accounts, load_cache, load_news_spec_from_file, load_pause_spec_from_file,
    load_spec_from_file, pick_pattern_interactive, pick_template_interactive,
    prompt_save_as_template, put_secret, record_account_use, record_prep_use, record_veto_use,
    run_calendar_bars, secret_binding_for, test_account, validate_instrument, wrap_signed,
    wrap_signed_template, write_news, write_pause, write_trade,
};
use trade_control_core::account::{
    AccountKind, AccountMetadata, Credentials, TradeNationCreds, TradeNationKind,
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
    /// Build a multi-alert trade from a chart pattern (H&S, IH&S, M, W).
    /// Runs a questionnaire, mints a shared `trade_id`, and emits 5
    /// signed alert YAMLs plus a manifest into the output directory.
    /// Each YAML is a complete TradingView-ready alert body — drop them
    /// into the matching TradingView alerts on the chart.
    BuildTrade(BuildTradeArgs),
    /// Build a `pause` + `resume` alert pair for a news-event blackout
    /// on an existing trade. The two alerts share a `blackout_id`; the
    /// worker keys its KV entry on `pause:<trade_id>:<blackout_id>`,
    /// and the `enter` gate rejects while any pause for the trade_id
    /// is active. Driven by a `pause.yaml` spec — `tv_arm_hs.py`
    /// writes one of these per pair of `blackout-start` /
    /// `blackout-end` vertical lines on the chart and shells out here.
    BuildPause(BuildPauseArgs),
    /// Build a `news-start` + `news-end` alert pair for a scheduled
    /// news window on an existing trade. The two alerts share a
    /// `news_id`; the worker keys its KV entry on
    /// `news:<trade_id>:<news_id>`. Unlike pause/resume, the entry
    /// gate is NOT affected — news windows enable a separate
    /// reversal-close intent that flattens the trade only when news
    /// is in play. Driven by a `news.yaml` spec — `tv_arm_hs.py`
    /// writes one per `news-start` / `news-end` vertical-line pair.
    BuildNews(BuildNewsArgs),
    /// Auto-emit pause + news alert pairs from `trade-calendar-maker`'s
    /// economic calendar. For each upcoming Medium+ (M15) or High (H1+)
    /// event affecting the instrument's currencies within the timeframe's
    /// buffer window, splits the event window in two: pause runs from
    /// `event - buffer_before` to `event`, news runs from `event` to
    /// `event + buffer_after`. Each event gets a deterministic id so
    /// re-running is idempotent and never collides with operator-drawn
    /// bars.
    CalendarBars(CalendarBarsArgs),
    /// Look up / manage broker instrument catalogs. Today only
    /// `--broker tradenation` is implemented; OANDA names round-trip
    /// cleanly through string mapping and don't need a catalog.
    #[command(subcommand)]
    Instruments(InstrumentsCmd),
    /// Print a shell completion script to stdout. Install with e.g.
    /// `trade-control completions zsh > ~/.zfunc/_trade-control`.
    Completions {
        /// Target shell.
        shell: Shell,
    },
}

#[derive(Subcommand)]
enum InstrumentsCmd {
    /// Force-refresh the on-disk catalog (re-walks the broker's market
    /// tree, ~1200 entries for TradeNation, takes a few seconds).
    Refresh {
        #[arg(long, value_enum, default_value_t = BrokerKindArg::TradeNation)]
        broker: BrokerKindArg,
    },
    /// Resolve a user-supplied name. Exit 0 on hit (prints canonical
    /// name + market_id), exit 2 on miss (prints candidate list to
    /// stderr).
    Resolve {
        /// Name to resolve, e.g. "XAG/USD", "Spot Gold", "EURUSD".
        name: String,
        #[arg(long, value_enum, default_value_t = BrokerKindArg::TradeNation)]
        broker: BrokerKindArg,
        /// Emit machine-readable JSON instead of plain text. Used by
        /// `tv_arm_hs.py` and other tooling.
        #[arg(long)]
        json: bool,
    },
    /// Print every known instrument name (one per line). Drives the zsh
    /// completion hook on the control subcommands' `instrument` positional.
    List {
        #[arg(long, value_enum, default_value_t = BrokerKindArg::TradeNation)]
        broker: BrokerKindArg,
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
    /// Skip the `wrangler secret put` step. Use when you've already set
    /// the credential secret manually, or you only want to write
    /// metadata first and provision credentials later.
    #[arg(long, default_value_t = false)]
    no_secret: bool,
    /// TradeNation: account username (the one that logs in). Prompted
    /// interactively if absent and the broker is TradeNation.
    #[arg(long)]
    username: Option<String>,
    /// OANDA: sub-account id (e.g. `101-011-31142393-003`). Prompted
    /// interactively if absent and the broker is OANDA. Stored on the
    /// account metadata — the shared worker-wide `OANDA_API_KEY`
    /// secret is the token used for all OANDA accounts.
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
    /// Broker the instrument belongs to. Defaults to OANDA to preserve
    /// existing scripts; pass `--broker tradenation` to validate the
    /// name against the TN catalog before sending.
    #[arg(long, value_enum, default_value_t = BrokerKindArg::Oanda)]
    broker: BrokerKindArg,
    /// Skip catalog validation and send the instrument string verbatim.
    /// Use when a non-canonical name is already stuck in KV (e.g.
    /// `XAUUSD.F`) and the canonical name won't match it.
    #[arg(long)]
    force: bool,
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
    /// Broker the instrument belongs to (see `unlock --broker`).
    #[arg(long, value_enum, default_value_t = BrokerKindArg::Oanda)]
    broker: BrokerKindArg,
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
    /// Broker the instrument belongs to (see `unlock --broker`).
    #[arg(long, value_enum, default_value_t = BrokerKindArg::Oanda)]
    broker: BrokerKindArg,
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
    /// Broker the instrument belongs to (see `unlock --broker`).
    #[arg(long, value_enum, default_value_t = BrokerKindArg::Oanda)]
    broker: BrokerKindArg,
    /// Skip catalog validation and send the instrument string verbatim.
    /// See `unlock --force` for the use case.
    #[arg(long)]
    force: bool,
    #[command(flatten)]
    common: EndpointArgs,
}

#[derive(Parser)]
struct ClearVetoCmdArgs {
    /// Instrument the veto applies to.
    instrument: String,
    /// Named veto to clear.
    name: String,
    /// Broker the instrument belongs to (see `unlock --broker`).
    #[arg(long, value_enum, default_value_t = BrokerKindArg::Oanda)]
    broker: BrokerKindArg,
    /// Skip catalog validation and send the instrument string verbatim.
    /// See `unlock --force` for the use case.
    #[arg(long)]
    force: bool,
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
struct BuildNewsArgs {
    /// Path to the `news.yaml` spec describing the news window.
    /// Required — there's no interactive mode (the inputs come from
    /// chart drawings, not a human questionnaire).
    #[arg(long)]
    from_file: PathBuf,
    /// Path to a hex-encoded 32-byte signing key.
    #[arg(long, env = "TRADE_CONTROL_KEY_FILE")]
    key_file: PathBuf,
    /// Directory to write the 2 alert YAMLs + manifest into. Created
    /// if missing. Default is `./news/<trade_id>/<news_id>/`
    /// resolved after the id is minted.
    #[arg(long)]
    output_dir: Option<PathBuf>,
}

#[derive(Parser)]
struct BuildPauseArgs {
    /// Path to the `pause.yaml` spec describing the blackout window.
    /// Required — there's no interactive mode (the inputs come from
    /// chart drawings, not a human questionnaire).
    #[arg(long)]
    from_file: PathBuf,
    /// Path to a hex-encoded 32-byte signing key. Same key the
    /// `build-trade` and `sign` paths use — pauses go through the
    /// same HMAC pipeline.
    #[arg(long, env = "TRADE_CONTROL_KEY_FILE")]
    key_file: PathBuf,
    /// Directory to write the 2 alert YAMLs + manifest into. Created
    /// if missing. Default is `./pauses/<trade_id>/<blackout_id>/`
    /// resolved after the id is minted.
    #[arg(long)]
    output_dir: Option<PathBuf>,
}

#[derive(Parser)]
struct BuildTradeArgs {
    /// Pattern to build (`hs`, `ihs`, `m`, `w`). Omit to fuzzy-pick
    /// interactively. Ignored when `--from-file` is set (the spec
    /// carries the pattern).
    pattern: Option<String>,
    /// Path to a hex-encoded 32-byte signing key. Same key used by
    /// `sign` and the intent endpoints — emitted alerts go through the
    /// usual HMAC pipeline.
    #[arg(long, env = "TRADE_CONTROL_KEY_FILE")]
    key_file: PathBuf,
    /// Directory to write the 5 alert YAMLs + manifest into. Created
    /// if missing. Default is `./trades/<trade_id>/` resolved after the
    /// id is minted.
    #[arg(long)]
    output_dir: Option<PathBuf>,
    /// Read a pre-filled `trade.yaml` and skip every prompt. The file's
    /// `pattern` field selects the build path; `pattern` positional and
    /// `--from-file` are mutually exclusive.
    #[arg(long, conflicts_with = "pattern")]
    from_file: Option<PathBuf>,
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
        Cmd::BuildTrade(args) => run_build_trade(args)?,
        Cmd::BuildPause(args) => run_build_pause(args)?,
        Cmd::BuildNews(args) => run_build_news(args)?,
        Cmd::CalendarBars(args) => {
            let key = load_key(&args.key_file)?;
            check_account_known(&args.account)?;
            run_calendar_bars(args, key, Utc::now())?;
        }
        Cmd::Instruments(sub) => run_instruments(sub)?,
        Cmd::Completions { shell } => run_completions(shell),
    }
    Ok(())
}

fn run_build_trade(args: BuildTradeArgs) -> Result<()> {
    let key = load_key(&args.key_file)?;
    let now = Utc::now();
    let trade = match args.from_file {
        Some(path) => {
            let spec = load_spec_from_file(&path)?;
            check_account_known(&spec.account)?;
            build_trade_from_spec(spec, now)?
        }
        None => {
            let pattern = match args.pattern {
                Some(s) => TradePattern::parse_arg(&s)
                    .ok_or_else(|| eyre!("unknown pattern {s:?} (expected hs / ihs / m / w)"))?,
                None => pick_pattern_interactive()?,
            };
            build_trade_interactive(pattern, now)?
        }
    };
    let out_dir = args
        .output_dir
        .unwrap_or_else(|| PathBuf::from("trades").join(&trade.trade_id));
    let written = write_trade(&trade, &key, &out_dir)?;
    println!("trade_id: {}", trade.trade_id);
    println!("output: {}", written.display());
    for alert in &trade.alerts {
        println!("  - {}.yaml — {}", alert.basename, alert.purpose);
    }
    Ok(())
}

fn run_build_news(args: BuildNewsArgs) -> Result<()> {
    let key = load_key(&args.key_file)?;
    let now = Utc::now();
    let spec = load_news_spec_from_file(&args.from_file)?;
    // Same account gate as build-pause / build-trade so an unregistered
    // account fails early. The worker's news-start / news-end handlers
    // never call the broker, but a typo would mean the parent trade
    // can't correlate its close-on-reversal intent later.
    check_account_known(&spec.account)?;
    let built = build_news_from_spec(spec, now)?;
    let out_dir = args.output_dir.unwrap_or_else(|| {
        PathBuf::from("news")
            .join(&built.trade_id)
            .join(&built.news_id)
    });
    let written = write_news(&built, &key, &out_dir)?;
    println!("trade_id: {}", built.trade_id);
    println!("news_id: {}", built.news_id);
    println!("output: {}", written.display());
    for alert in &built.alerts {
        println!("  - {}.yaml — {}", alert.basename, alert.purpose);
    }
    Ok(())
}

fn run_build_pause(args: BuildPauseArgs) -> Result<()> {
    let key = load_key(&args.key_file)?;
    let now = Utc::now();
    let spec = load_pause_spec_from_file(&args.from_file)?;
    // Reuse the build-trade gate so the operator can't accidentally
    // route a pause at an account they haven't registered. Pauses
    // don't touch the broker but a typo here would still leave the
    // worker scratching its head when the parent enter fires.
    check_account_known(&spec.account)?;
    let built = build_pause_from_spec(spec, now)?;
    let out_dir = args.output_dir.unwrap_or_else(|| {
        PathBuf::from("pauses")
            .join(&built.trade_id)
            .join(&built.blackout_id)
    });
    let written = write_pause(&built, &key, &out_dir)?;
    println!("trade_id: {}", built.trade_id);
    println!("blackout_id: {}", built.blackout_id);
    println!("output: {}", written.display());
    for alert in &built.alerts {
        println!("  - {}.yaml — {}", alert.basename, alert.purpose);
    }
    Ok(())
}

/// Reject the spec early if its `account` isn't in the local history
/// cache (`~/.config/trade-control/history.yaml`). The cache is
/// populated by `trade-control account list` / `account test`. An
/// empty cache means the operator has never run those — we let the
/// build proceed in that case rather than block on a missing cache.
fn check_account_known(account: &str) -> Result<()> {
    let history = trade_control_cli::load_history();
    let known = history.account_names();
    if known.is_empty() {
        return Ok(());
    }
    if known.iter().any(|n| n == account) {
        return Ok(());
    }
    Err(eyre!(
        "account {account:?} is not in the local cache. Known accounts: {}. \
         Run `trade-control account list` to refresh, or fix the spec.",
        known.join(", ")
    ))
}

fn run_completions(shell: Shell) {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "trade-control", &mut io::stdout());
    // For zsh, append a dynamic completer that fills the `instrument`
    // positional on control subcommands with the live TradeNation catalog
    // when --broker tradenation is in argv. Other shells get the static
    // clap output; the catalog walk is too slow to be useful as a
    // bash/fish completer that runs on every TAB.
    if matches!(shell, Shell::Zsh) {
        print!("{ZSH_TN_INSTRUMENTS_HOOK}");
    }
}

/// Hand-rolled zsh helper appended after the clap-generated script. Defines
/// `_trade_control_tn_instruments`, which fetches live TradeNation names
/// when `--broker tradenation` is present in the current argv.
///
/// clap's generated zsh marks the `instrument` positional with an empty
/// action (no completer), so this function isn't automatically hooked in.
/// To wire it up, add to your zshrc *after* sourcing the trade-control
/// completion file:
///
/// ```zsh
/// # Use the helper for any trade-control subcommand whose first positional
/// # is named "instrument". -e replaces the existing rule.
/// for sub in unlock prep veto clear-prep clear-veto; do
///     compdef -e "_arguments -S '(-h --help)'{-h,--help}'[show help]' \
///         ':instrument:_trade_control_tn_instruments' '*::: :->args'" \
///         "trade-control $sub"
/// done
/// ```
///
/// (We don't write this dispatch ourselves because clap regenerates the
/// completion on every flag rename — keeping our hook independent means
/// the user's override survives upstream changes.)
const ZSH_TN_INSTRUMENTS_HOOK: &str = r#"
# trade-control: TradeNation instrument-name completer for control
# subcommands. See the run_completions doc comment in the source for
# wiring instructions.
_trade_control_tn_instruments() {
    # Bail out unless `--broker tradenation` is in argv, so OANDA users
    # aren't shown a TN-only list.
    local has_tn=0 i
    for ((i = 2; i <= ${#words}; i++)); do
        if [[ ${words[i]} == "--broker=tradenation" ]]; then
            has_tn=1
            break
        fi
        if [[ ${words[i]} == "--broker" && ${words[i+1]} == "tradenation" ]]; then
            has_tn=1
            break
        fi
    done
    (( has_tn )) || return 1

    local -a names
    names=("${(@f)$(trade-control instruments list --broker tradenation 2>/dev/null)}")
    compadd -- "${names[@]}"
}
"#;

fn run_instruments(sub: InstrumentsCmd) -> Result<()> {
    match sub {
        InstrumentsCmd::Refresh { broker } => match broker.into() {
            BrokerKind::TradeNation => {
                let cache = load_cache(true, None)?;
                println!(
                    "refreshed: {} markets at {}",
                    cache.catalog().markets.len(),
                    cache.path().display(),
                );
                Ok(())
            }
            BrokerKind::Oanda => Err(eyre!(
                "OANDA catalog is not implemented yet; only --broker tradenation is supported",
            )),
        },
        InstrumentsCmd::Resolve { name, broker, json } => match broker.into() {
            BrokerKind::TradeNation => run_resolve_tradenation(&name, json),
            BrokerKind::Oanda => Err(eyre!(
                "OANDA catalog is not implemented yet; only --broker tradenation is supported",
            )),
        },
        InstrumentsCmd::List { broker } => match broker.into() {
            BrokerKind::TradeNation => {
                let cache = load_cache(false, None)?;
                for name in cache.names() {
                    println!("{name}");
                }
                Ok(())
            }
            BrokerKind::Oanda => Err(eyre!(
                "OANDA catalog is not implemented yet; only --broker tradenation is supported",
            )),
        },
    }
}

/// Resolve `name` against the TN cache and print the result. `json = true`
/// uses a machine-readable shape consumed by `tv_arm_hs.py`. Exit code 2 on
/// miss (distinct from "broker / cache failed" → exit 1) so callers can
/// distinguish a clean "not found" from an infra problem.
fn run_resolve_tradenation(name: &str, json: bool) -> Result<()> {
    use tradenation_instrument_cache::ResolveError;

    let cache = load_cache(false, None)?;
    match cache.resolve(name) {
        Ok(market) => {
            if json {
                let payload = serde_json::json!({
                    "ok": true,
                    "name": market.name,
                    "market_id": market.market_id,
                    "symbol": market.symbol,
                    "currency": market.currency,
                });
                println!("{payload}");
            } else {
                println!("name:      {}", market.name);
                println!("market_id: {}", market.market_id);
                if let Some(sym) = &market.symbol {
                    println!("symbol:    {sym}");
                }
                println!("currency:  {}", market.currency);
            }
            Ok(())
        }
        Err(ResolveError::NotFound { query, candidates }) => {
            if json {
                let cands: Vec<_> = candidates
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "name": c.name,
                            "market_id": c.market_id,
                            "symbol": c.symbol,
                        })
                    })
                    .collect();
                let payload = serde_json::json!({
                    "ok": false,
                    "query": query,
                    "candidates": cands,
                });
                println!("{payload}");
            } else {
                eprintln!("no match for {query:?}");
                if candidates.is_empty() {
                    eprintln!("  no candidates");
                } else {
                    eprintln!("  did you mean:");
                    for c in &candidates {
                        eprintln!("    - {}", c.name);
                    }
                }
            }
            std::process::exit(2);
        }
    }
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
    if !response.ends_with('\n') {
        println!();
    }
    if let Some(footer) = build_status_instruments_footer(&response) {
        print!("{footer}");
    }
    Ok(())
}

/// Build a `# instruments:` footer that annotates each unique instrument
/// string in the snapshot with its canonical TN name (if any). Returns
/// `None` when the response can't be parsed or carries no instrument
/// fields, so the status output stays a plain pass-through in those
/// cases.
///
/// Names that resolve cleanly through the TN catalog are tagged
/// `→ Canonical Name`. Names that don't resolve AND don't look like
/// OANDA identifiers (which always contain `_`) are tagged
/// `(no TN catalog match)` — that's the case that catches stranded
/// strings like `XAUUSD.F` so the operator knows to reach for
/// `clear-veto --force` instead of guessing at canonical names.
fn build_status_instruments_footer(response: &str) -> Option<String> {
    use trade_control_core::state::Snapshot;

    let snap: Snapshot = serde_yaml::from_str(response).ok()?;

    let mut names: Vec<String> = snap
        .cooldowns
        .iter()
        .map(|c| c.instrument.clone())
        .chain(snap.preps.iter().map(|p| p.instrument.clone()))
        .chain(snap.vetos.iter().map(|v| v.instrument.clone()))
        .collect();
    names.sort();
    names.dedup();

    if names.is_empty() {
        return None;
    }

    // Best-effort cache load. If TN login or disk read fails we still
    // print the section header + raw names so the operator sees which
    // strings exist, but without a `→` annotation.
    let cache = load_cache(false, None).ok();
    Some(format_instruments_footer(&names, cache.as_ref()))
}

/// Pure format helper for [`build_status_instruments_footer`]. Split
/// out so tests can drive it with a seeded `InstrumentCache` instead
/// of needing a live TN session.
fn format_instruments_footer(
    names: &[String],
    cache: Option<&tradenation_instrument_cache::InstrumentCache>,
) -> String {
    let mut out = String::from("# instruments:\n");
    for name in names {
        let annotation = annotate_instrument(cache, name);
        out.push_str(&format!("#   - {name}{annotation}\n"));
    }
    out
}

/// Annotation suffix for one stored instrument string.
/// - Empty string when `cache` is `None` (offline) or when the name
///   already matches the canonical TN form.
/// - ` → Canonical Name` when TN catalog resolution succeeded with
///   a different name (e.g. `EUR_USD` → `EUR/USD`, `XAGUSD` →
///   `Spot Silver`). Tells the operator what to type for
///   `clear-veto` / `clear-prep` / `unlock`.
/// - ` (no TN catalog match)` when the catalog couldn't resolve the
///   string at all. That's the stuck-key case (e.g. `XAUUSD.F`):
///   `clear-veto` will reject the name; operator needs `--force`.
///   Also covers OANDA-exclusive names that don't exist on TN — they
///   self-identify by failing here.
fn annotate_instrument(
    cache: Option<&tradenation_instrument_cache::InstrumentCache>,
    name: &str,
) -> String {
    let Some(cache) = cache else {
        return String::new();
    };
    match cache.resolve(name) {
        Ok(market) => {
            if market.name.eq_ignore_ascii_case(name.trim()) {
                String::new()
            } else {
                format!(" → {}", market.name)
            }
        }
        Err(_) => " (no TN catalog match)".to_string(),
    }
}

/// Validate `instrument` against `broker`'s catalog, returning the canonical
/// name (or the input verbatim when the broker has no catalog / the name
/// already matched exactly). On miss this returns the cache's
/// "did you mean ..." error, which the caller propagates so the worker
/// never sees an unknown instrument.
fn canonicalize_instrument(broker: BrokerKindArg, instrument: &str) -> Result<String> {
    let broker_kind: BrokerKind = broker.into();
    match validate_instrument(broker_kind, instrument)? {
        Some(canonical) => {
            tracing::warn!(
                input = %instrument,
                canonical = %canonical,
                "instrument resolved to canonical broker name",
            );
            Ok(canonical)
        }
        None => Ok(instrument.to_string()),
    }
}

/// Same as [`canonicalize_instrument`] but returns the input verbatim
/// when `force` is set. Used by `unlock --force`, `clear-prep --force`,
/// and `clear-veto --force` to clear stranded non-canonical keys that
/// the catalog can't resolve.
fn canonicalize_instrument_or_force(
    broker: BrokerKindArg,
    instrument: &str,
    force: bool,
) -> Result<String> {
    if force {
        tracing::warn!(
            input = %instrument,
            "--force: sending instrument verbatim, skipping catalog validation",
        );
        return Ok(instrument.to_string());
    }
    canonicalize_instrument(broker, instrument)
}

fn run_unlock(args: UnlockCmdArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let instrument = canonicalize_instrument_or_force(args.broker, &args.instrument, args.force)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_unlock_intent(&instrument, now, &suffix);
    let body = wrap_control(&intent, &key, now)?;
    let response = post_control(&args.common.endpoint, &body)?;
    print!("{response}");
    Ok(())
}

fn run_prep(args: PrepCmdArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let instrument = canonicalize_instrument(args.broker, &args.instrument)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_prep_intent(
        &instrument,
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
    let instrument = canonicalize_instrument(args.broker, &args.instrument)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    // Default level is sent as `None` to keep the wire form minimal —
    // the worker treats absent and `stop-next-entry` identically.
    let level: Option<VetoLevel> = match args.level {
        VetoLevelArg::StopNextEntry => None,
        other => Some(other.into()),
    };
    let intent = build_veto_intent(
        &instrument,
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
    let instrument = canonicalize_instrument_or_force(args.broker, &args.instrument, args.force)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_clear_prep_intent(&instrument, &args.step, now, &suffix);
    let body = wrap_control(&intent, &key, now)?;
    let response = post_control(&args.common.endpoint, &body)?;
    print!("{response}");
    Ok(())
}

fn run_clear_veto(args: ClearVetoCmdArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let instrument = canonicalize_instrument_or_force(args.broker, &args.instrument, args.force)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_clear_veto_intent(&instrument, &args.name, now, &suffix);
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
    // Side-effect: warm the sign-flow auto-complete cache with every
    // canonical name the worker reports. Best-effort — if the response
    // isn't the expected shape we silently skip the cache update and
    // still print the body.
    cache_account_names_from_list(&body);
    print!("{body}");
    if !body.ends_with('\n') {
        println!();
    }
    Ok(())
}

/// Parse the YAML body returned by `GET /admin/accounts` and record
/// every `name:` we find in the operator's local history. Failures are
/// swallowed — auto-complete is a convenience, not a correctness gate.
fn cache_account_names_from_list(body: &str) {
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(body) else {
        return;
    };
    let Some(seq) = value.as_sequence() else {
        return;
    };
    for entry in seq {
        if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
            record_account_use(name);
        }
    }
}

fn run_account_test(args: AccountTestArgs) -> Result<()> {
    let admin_key = load_admin_key(&args.common.admin_key_file)?;
    let body = test_account(&args.common.endpoint, &admin_key, &args.name)?;
    // `test_account` errored out if the worker rejected — a successful
    // return means the name resolves. Cache it so the next `sign`
    // offers it as an auto-complete option.
    record_account_use(&args.name);
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
    let theme = ColorfulTheme::default();

    // Caps — only attach if either field was supplied; otherwise default
    // skip-serialise keeps the wire form minimal.
    let caps = trade_control_core::account::AccountCaps {
        max_risk_pct: args.max_risk_pct,
        max_open_positions: args.max_open_positions,
    };

    // OANDA: capture the sub-account id (lives on metadata, not as a
    // secret — there's no per-account OANDA token, just one shared
    // worker-wide `OANDA_API_KEY` that covers every sub-account).
    // TradeNation: skipped; the account is identified by login creds.
    let oanda_account_id = match broker {
        BrokerKind::Oanda => {
            let id = match args.account_id {
                Some(id) => id,
                None => Input::with_theme(&theme)
                    .with_prompt("OANDA sub-account id (e.g. 101-011-XXXXXXXX-003)")
                    .interact_text()
                    .map_err(|e| eyre!("read account id: {e}"))?,
            };
            Some(id)
        }
        BrokerKind::TradeNation => None,
    };

    let metadata = AccountMetadata {
        name: args.name.clone(),
        broker,
        kind,
        caps,
        oanda_account_id,
    };

    // Build credentials *first* so we fail before touching the worker
    // if the operator aborts at the prompt. The credential half is the
    // expensive thing to redo. OANDA accounts skip this entirely —
    // they reuse the shared worker-wide `OANDA_API_KEY` and don't
    // need a per-account secret.
    let tn_creds = if args.no_secret || broker != BrokerKind::TradeNation {
        None
    } else {
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
    };

    // Write metadata first. If this fails (e.g. already-exists) we
    // haven't touched secrets — the operator can re-run.
    let add_body = add_account(&args.common.endpoint, &admin_key, &metadata)?;
    // Cache the name for sign-flow auto-complete now that the worker
    // has accepted it.
    record_account_use(&args.name);
    print!("{add_body}");
    if !add_body.ends_with('\n') {
        println!();
    }

    // Then push the credential to Secret Store (TradeNation only).
    // The two-step shape is deliberate — a failure here leaves the
    // operator with a metadata entry pointing at a missing secret,
    // which `account test` will surface. They can then re-run with
    // `--no-secret` skipped to retry the secret-put alone.
    match (broker, tn_creds) {
        (BrokerKind::TradeNation, Some(creds)) => {
            let binding = secret_binding_for(broker, &args.name);
            eprintln!("uploading credential secret {binding} via wrangler…");
            put_secret(&binding, &creds)?;
        }
        (BrokerKind::TradeNation, None) => {
            eprintln!(
                "skipped credential secret (use `wrangler secret put {}` to set it)",
                secret_binding_for(broker, &args.name)
            );
        }
        (BrokerKind::Oanda, _) => {
            eprintln!(
                "oanda accounts share the worker-wide OANDA_API_KEY — no per-account secret needed"
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tradenation_api::{Market, Session};
    use tradenation_instrument_cache::{Catalog, InstrumentCache};

    fn stub_market(market_id: u64, name: &str, symbol: Option<&str>) -> Market {
        Market {
            market_id,
            quote_id: 1,
            name: name.to_string(),
            currency: "USD".to_string(),
            super_group_id: 1,
            spread: 0.0,
            margin: 0.0,
            bet_per: 0.0,
            decimal_places: 2,
            tradable: true,
            trade_on_web: true,
            bid: 0.0,
            ask: 0.0,
            symbol: symbol.map(str::to_string),
        }
    }

    /// Build an `InstrumentCache` seeded from `markets`. Mirrors the
    /// `seeded_cache` helper in `instruments.rs` so this test module
    /// stays self-contained.
    fn seeded_cache(markets: Vec<Market>) -> (InstrumentCache, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("catalog.json");
        let cat = Catalog::new(markets);
        std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cache = rt
            .block_on(async {
                let session = Session::demo("x", "x", "x", None);
                InstrumentCache::load_or_fetch(
                    &session,
                    Duration::from_secs(3600),
                    Some(path.clone()),
                )
                .await
            })
            .unwrap();
        (cache, tmp)
    }

    #[test]
    fn annotate_oanda_name_resolves_via_tn_normalize() {
        // EUR_USD lands on TN's `EUR/USD` via the catalog's
        // normalize-alphanumeric tier (strips `_`, matches symbol
        // `EURUSD`). Surface the canonical so the operator knows
        // what string to type for clear-veto.
        let (cache, _tmp) = seeded_cache(vec![stub_market(1, "EUR/USD", Some("EURUSD"))]);
        assert_eq!(annotate_instrument(Some(&cache), "EUR_USD"), " → EUR/USD");
    }

    #[test]
    fn annotate_unresolved_name_is_flagged() {
        let (cache, _tmp) = seeded_cache(vec![stub_market(1, "Spot Gold", None)]);
        // The bug case: KV stored XAUUSD.F, TN catalog has no symbol
        // for Spot Gold, so this can't resolve.
        assert_eq!(
            annotate_instrument(Some(&cache), "XAUUSD.F"),
            " (no TN catalog match)"
        );
    }

    #[test]
    fn annotate_canonical_name_is_silent() {
        let (cache, _tmp) = seeded_cache(vec![stub_market(1, "Spot Gold", None)]);
        // Already canonical — no rewrite needed.
        assert_eq!(annotate_instrument(Some(&cache), "Spot Gold"), "");
    }

    #[test]
    fn annotate_symbol_resolves_to_canonical() {
        let (cache, _tmp) = seeded_cache(vec![stub_market(1, "EUR/USD", Some("EURUSD"))]);
        // Operator (or some upstream) used the symbol — show the
        // canonical name so they know what to type next.
        assert_eq!(annotate_instrument(Some(&cache), "EURUSD"), " → EUR/USD");
    }

    #[test]
    fn annotate_no_cache_is_silent() {
        // When TN login fails the footer still prints the name list;
        // each line just has no annotation suffix.
        assert_eq!(annotate_instrument(None, "XAUUSD.F"), "");
        assert_eq!(annotate_instrument(None, "EUR_USD"), "");
    }

    #[test]
    fn footer_formats_unique_sorted_names() {
        let (cache, _tmp) = seeded_cache(vec![
            stub_market(1, "Spot Gold", None),
            stub_market(2, "EUR/USD", Some("EURUSD")),
        ]);
        // Mixed bag: canonical TN, OANDA-style, symbol, stranded.
        let names = vec![
            "EUR_USD".to_string(),
            "EURUSD".to_string(),
            "Spot Gold".to_string(),
            "XAUUSD.F".to_string(),
        ];
        let footer = format_instruments_footer(&names, Some(&cache));
        let expected = "\
# instruments:
#   - EUR_USD → EUR/USD
#   - EURUSD → EUR/USD
#   - Spot Gold
#   - XAUUSD.F (no TN catalog match)
";
        assert_eq!(footer, expected);
    }
}
