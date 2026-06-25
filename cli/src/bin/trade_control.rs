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
    AdoptBody, BuildStrictness, CalendarBarsArgs, KEY_LEN, TradePattern, add_account, adopt_trade,
    build_clear_prep_intent, build_clear_veto_intent, build_market_info_intent,
    build_news_from_spec, build_pause_from_spec, build_plan_delete_intent, build_plan_list_intent,
    build_plan_show_intent, build_prep_intent, build_status_intent, build_trade_from_spec,
    build_trade_interactive, build_unlock_intent, build_veto_intent, delete_account, delete_secret,
    fill_missing_fields, generate_key_hex, list_accounts, load_cache, load_news_spec_from_file,
    load_pause_spec_from_file, load_spec_from_file, pick_pattern_interactive,
    pick_template_interactive, prompt_save_as_template, put_secret, record_account_use,
    record_prep_use, record_veto_use, require_local_tn_account, run_calendar_bars,
    secret_binding_for, test_account, validate_instrument, wrap_signed, wrap_signed_template,
    write_news, write_pause, write_trade,
};
use trade_control_core::account::{
    AccountKind, AccountMetadata, Credentials, TradeNationCreds, TradeNationKind,
};
use trade_control_core::incoming::signed_pairs_from_text;
use trade_control_core::intent::Intent;
use trade_control_core::intent::{BrokerKind, Direction, VetoLevel};
use trade_control_core::sig::{self, SIG_FIELD};

#[derive(Parser)]
#[command(
    name = "trade-control",
    version = env!("GIT_VERSION"),
    about = "Sign a trade intent for TradingView and manage worker state"
)]
struct Cli {
    /// Print a shell completion script for the current shell (detected
    /// from `$SHELL`) to stdout, then exit. Designed for `eval`:
    /// `eval "$(trade-control --print-completions)"` in your shell rc.
    /// For an explicit shell or to write to a file, use the
    /// `completions <shell>` subcommand instead.
    #[arg(long, exclusive = true)]
    print_completions: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
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
    /// Show TradeNation trading hours + market details for one instrument.
    /// Queries the worker, which resolves the instrument against the broker
    /// and returns its session hours (Brisbane + London), spread, margin,
    /// guaranteed-stop terms and expiry. TradeNation-only.
    #[command(alias = "hours")]
    MarketInfo(MarketInfoCmdArgs),
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
    /// Adopt an externally-opened broker position into worker
    /// management. Posts to `/admin/adopt-trade`; the worker verifies
    /// the position against the live broker before writing the
    /// `EntryAttempt` row. After adopt, every other worker path
    /// (close, pause/resume, retry-gate, SL-breach sweep) treats the
    /// trade as if the worker had placed it itself. Use after opening
    /// a trade manually in the broker UI when you want the webhook
    /// lifecycle to run against it.
    AdoptTrade(AdoptTradeArgs),
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
    /// Inspect / manage the server-side engine's registered `TradePlan`s.
    /// `plan list` and `plan show <id>` are read-only queries; `plan delete
    /// <id>` drops a plan (the inverse of register) so a setup can be
    /// re-armed after editing the chart. Useful during the engine's
    /// parallel-run period to confirm a plan registered, whether it's in
    /// shadow mode, and how far its FSM has progressed.
    #[command(subcommand)]
    Plan(PlanCmd),
    /// Print a shell completion script to stdout. Install with e.g.
    /// `trade-control completions zsh > ~/.zfunc/_trade-control`.
    Completions {
        /// Target shell.
        shell: Shell,
    },
}

#[derive(Subcommand)]
enum PlanCmd {
    /// List every registered plan with a compact summary of its current
    /// engine state (phase, watermark, fired rules, shadow flag).
    List(PlanListArgs),
    /// Dump one plan in full — every rule plus its persisted engine state.
    /// The worker scans all account scopes for the `trade_id`.
    Show(PlanShowArgs),
    /// Delete a registered plan and its engine state — the inverse of
    /// register. The worker scans all account scopes and drops the matching
    /// `plan:` + `plan-state:` rows. Idempotent (deleting a missing plan is a
    /// no-op). Use to re-arm a setup after editing its chart: `plan delete
    /// <id>` then re-run `tv-arm`.
    Delete(PlanDeleteArgs),
}

#[derive(Parser)]
struct PlanListArgs {
    /// Also list terminated plans (vetoed / completed). By default only live
    /// plans the engine is still ticking are shown; terminated plans are
    /// archived to a separate keyspace on the terminal cron tick and surfaced
    /// only with this flag. Use it to analyze a setup after it vetoed, then
    /// `plan delete <id>` to drop the archive.
    #[arg(long, visible_alias = "include-archived")]
    include_all: bool,
    /// Print the worker's raw YAML response instead of the table.
    #[arg(long)]
    yaml: bool,
    #[command(flatten)]
    common: EndpointArgs,
}

#[derive(Parser)]
struct PlanShowArgs {
    /// The plan's `trade_id` (e.g. `eurusd-hs-7`).
    trade_id: String,
    /// Print the worker's raw YAML response instead of the pretty view.
    #[arg(long)]
    yaml: bool,
    #[command(flatten)]
    common: EndpointArgs,
}

#[derive(Parser)]
struct PlanDeleteArgs {
    /// The plan's `trade_id` (e.g. `eurusd-hs-7`).
    trade_id: String,
    #[command(flatten)]
    common: EndpointArgs,
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
    /// Print every account name known locally — union of the operator
    /// history (populated by `account list` / `add` / `test`) and the
    /// local TN store. One per line, no auth, no network. Intended for
    /// shell tab-completion of `--account-id`.
    #[command(hide = true)]
    Names,
}

#[derive(Parser)]
struct AccountEndpointArgs {
    /// Path to a file containing the `ADMIN_KEY` secret (hex or plain
    /// text — read verbatim, trimmed). Distinct from `--key-file` so a
    /// leaked intent-auth key can't pivot to account mutation.
    #[arg(long, env = "TRADE_CONTROL_ADMIN_KEY_FILE")]
    admin_key_file: PathBuf,
    /// Worker URL (e.g. https://trade-control.<account>.workers.dev).
    /// Precedence: this flag > `TRADE_CONTROL_ENDPOINT` env > the
    /// compiled-in default baked by `build.rs` from `TRADE_CONTROL_WEBHOOK`
    /// at build time (the dev URL for a plain `cargo install`). This is why
    /// the per-environment binaries (`trade-control-staging`, `-dev`, …)
    /// work standalone with no env var set.
    #[arg(long, env = "TRADE_CONTROL_ENDPOINT", default_value = env!("BAKED_WEBHOOK"))]
    endpoint: String,
    /// Cloudflare Worker name for `wrangler secret put/delete`. Without
    /// it, wrangler reads the name from a `wrangler.toml` in the current
    /// dir — so `account add`/`delete` only work from the repo root.
    /// Passing it makes those commands cwd-independent.
    #[arg(
        long,
        env = "TRADE_CONTROL_WORKER_NAME",
        default_value = "trade-control-web-hook"
    )]
    worker_name: String,
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
    /// TradeNation: account username (the one that logs in). Override
    /// for the default behavior, which pulls username + password from
    /// the local TN store (`~/.config/tradenation/accounts.enc`).
    /// When set, prompts for a fresh password instead of reading
    /// either field from the store.
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

#[derive(Parser)]
struct AdoptTradeArgs {
    /// Worker account name the position lives under, e.g.
    /// `tn-reversals-demo`. Must already be registered via
    /// `account add`.
    #[arg(long)]
    account: String,
    /// Trade grouping id minted by the original alert pipeline (e.g.
    /// `hs-chf-jpy-efd5e647`). The string from the `build-trade`
    /// manifest. Subsequent close / pause / news alerts for this
    /// trade must use the same id.
    #[arg(long)]
    trade_id: String,
    /// Broker market name as displayed by the broker UI, e.g.
    /// `CHF/JPY` or `Spot Gold`. Matched case-insensitively against
    /// the live position's `MarketName`.
    #[arg(long)]
    instrument: String,
    /// Trade direction: `long` (broker Buy) or `short` (broker Sell).
    #[arg(long, value_enum)]
    direction: DirectionArg,
    /// Originating order id from the broker UI (TradeNation shows
    /// it as `Add Order` on the trade detail panel).
    #[arg(long)]
    order_id: String,
    /// Open position id from the broker UI (`Open Position`).
    /// On TradeNation this is distinct from the order id; on OANDA
    /// they happen to be equal.
    #[arg(long)]
    position_id: String,
    /// Resolved stop-loss price. Optional, but recommended — without
    /// it the cron SL-breach sweep can't act on the adopted trade.
    #[arg(long)]
    stop_loss: Option<f64>,
    #[command(flatten)]
    common: AccountEndpointArgs,
}

/// Clap-side mirror of [`Direction`]. Same pattern as
/// [`BrokerKindArg`] — keeps the `clap` derive out of `core`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
enum DirectionArg {
    Long,
    Short,
}

impl From<DirectionArg> for Direction {
    fn from(v: DirectionArg) -> Self {
        match v {
            DirectionArg::Long => Direction::Long,
            DirectionArg::Short => Direction::Short,
        }
    }
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
    /// Precedence: this flag > `TRADE_CONTROL_ENDPOINT` env > the
    /// compiled-in default baked by `build.rs` from `TRADE_CONTROL_WEBHOOK`
    /// at build time (the dev URL for a plain `cargo install`). This is why
    /// the per-environment binaries (`trade-control-staging`, `-dev`, …)
    /// work standalone with no env var set.
    #[arg(long, env = "TRADE_CONTROL_ENDPOINT", default_value = env!("BAKED_WEBHOOK"))]
    endpoint: String,
}

#[derive(Parser)]
struct MarketInfoCmdArgs {
    /// Instrument to look up, e.g. "Wall Street 30", US30, or EUR_USD.
    /// Resolved to the canonical TradeNation MarketName via the catalog
    /// (unless --force), then the worker resolves it against the broker.
    instrument: String,
    /// Skip catalog validation and send the instrument string verbatim
    /// (use when the catalog can't resolve a name the broker still knows).
    #[arg(long)]
    force: bool,
    #[command(flatten)]
    common: EndpointArgs,
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
    /// Trade setup this veto belongs to. The veto is scoped to this
    /// trade_id so it only blocks entries from the same setup — it
    /// never bleeds into a later, independent trade on the same
    /// instrument.
    #[arg(long)]
    trade_id: String,
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
    /// Account scope the prep was set under. Preps are keyed by
    /// `(account, instrument, step)`; omit for a global (`_`) prep, or pass
    /// the account name (e.g. `reversals`) to clear an account-scoped one.
    /// `status` shows each prep's `account:` field — match it exactly.
    #[arg(long)]
    account: Option<String>,
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
    /// Trade setup the veto belongs to. Must match the trade_id the
    /// veto was set under — clearing is scoped per-setup.
    #[arg(long)]
    trade_id: String,
    /// Account scope the veto was set under. Vetos are keyed by
    /// `(account, trade_id, instrument, name)`; omit for a global (`_`)
    /// veto, or pass the account name to clear an account-scoped one.
    /// `status` shows each veto's `account:` field — match it exactly.
    #[arg(long)]
    account: Option<String>,
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
    if cli.print_completions {
        run_completions(detect_shell()?);
        return Ok(());
    }
    let cmd = cli
        .cmd
        .ok_or_else(|| eyre!("no subcommand given (try `trade-control --help`)"))?;
    match cmd {
        Cmd::GenKey => {
            let hex_key = generate_key_hex();
            println!("{hex_key}");
        }
        Cmd::Sign(args) => run_sign(args)?,
        Cmd::Status(args) => run_status(args)?,
        Cmd::MarketInfo(args) => run_market_info(args)?,
        Cmd::Unlock(args) => run_unlock(args)?,
        Cmd::Prep(args) => run_prep(args)?,
        Cmd::Veto(args) => run_veto(args)?,
        Cmd::ClearPrep(args) => run_clear_prep(args)?,
        Cmd::ClearVeto(args) => run_clear_veto(args)?,
        Cmd::Verify(args) => run_verify(args)?,
        Cmd::Account(sub) => run_account(sub)?,
        Cmd::AdoptTrade(args) => run_adopt_trade(args)?,
        Cmd::BuildTrade(args) => run_build_trade(args)?,
        Cmd::BuildPause(args) => run_build_pause(args)?,
        Cmd::BuildNews(args) => run_build_news(args)?,
        Cmd::CalendarBars(args) => {
            let key = load_key(&args.key_file)?;
            check_account_known(&args.account)?;
            run_calendar_bars(args, key, Utc::now(), None)?;
        }
        Cmd::Instruments(sub) => run_instruments(sub)?,
        Cmd::Plan(sub) => run_plan(sub)?,
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
            // `build-trade` signs a bundle bound for the live worker, so the
            // strict checks (trade_expiry in the future, etc.) stay on.
            build_trade_from_spec(spec, now, BuildStrictness::Strict)?
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

/// Detect the running shell from `$SHELL` for `--print-completions`.
/// `$SHELL` is the login shell (e.g. `/usr/bin/zsh`); we take the
/// basename and let clap_complete parse it. Falls back to a clear error
/// listing the supported shells so the operator can use the explicit
/// `completions <shell>` subcommand instead.
fn detect_shell() -> Result<Shell> {
    let shell_path = std::env::var("SHELL")
        .map_err(|_| eyre!("$SHELL is not set — run `completions <shell>`"))?;
    shell_from_path(&shell_path)
}

/// Parse a `Shell` from a `$SHELL`-style path by taking its basename
/// (e.g. `/usr/bin/zsh` → `zsh`). Split out from [`detect_shell`] so the
/// path-parsing logic is testable without mutating the process env.
fn shell_from_path(shell_path: &str) -> Result<Shell> {
    let name = std::path::Path::new(shell_path)
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| eyre!("could not read shell name from $SHELL={shell_path:?}"))?;
    name.parse::<Shell>().map_err(|_| {
        eyre!("unsupported shell {name:?} from $SHELL — run `completions <shell>` with one of: bash, zsh, fish, elvish, powershell")
    })
}

/// The name the completion script should bind to. Uses the actual invoked
/// binary's file stem (`argv[0]`) so a renamed-on-install copy
/// (`trade-control-staging`, `trade-control-dev`) emits completions for
/// *its own* name rather than the static `trade-control`. Falls back to
/// `trade-control` if argv[0] is unreadable.
fn invoked_name() -> String {
    std::env::args()
        .next()
        .and_then(|a| {
            std::path::Path::new(&a)
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "trade-control".to_string())
}

fn run_completions(shell: Shell) {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, invoked_name(), &mut io::stdout());
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
                // No --account-id on this admin command; fall back to
                // the default-demo login. The catalog is account-
                // agnostic, so any working demo produces the same data.
                let cache = load_cache(true, None, None)?;
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
                let cache = load_cache(false, None, None)?;
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

    let cache = load_cache(false, None, None)?;
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

fn run_plan(sub: PlanCmd) -> Result<()> {
    match sub {
        PlanCmd::List(args) => run_plan_list(args),
        PlanCmd::Show(args) => run_plan_show(args),
        PlanCmd::Delete(args) => run_plan_delete(args),
    }
}

/// `plan list` — query the worker for every registered plan. Pretty table by
/// default; `--yaml` passes the worker's raw YAML through verbatim.
fn run_plan_list(args: PlanListArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_plan_list_intent(now, &suffix, args.include_all);
    let body = wrap_control(&intent, &key, now)?;
    let response = post_control(&args.common.endpoint, &body)?;
    if args.yaml {
        print_raw(&response);
        return Ok(());
    }
    match serde_yaml::from_str::<Vec<serde_yaml::Value>>(&response) {
        Ok(plans) => print!("{}", format_plan_list(&plans)),
        // Unexpected shape / error body — don't hide it.
        Err(_) => print_raw(&response),
    }
    Ok(())
}

/// `plan show <trade_id>` — full dump of one plan + its engine state. Pretty
/// key-value view by default; `--yaml` for the raw worker YAML.
fn run_plan_show(args: PlanShowArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_plan_show_intent(&args.trade_id, now, &suffix);
    let body = wrap_control(&intent, &key, now)?;
    // A miss is a 404; `post_control` turns any non-2xx into an `Err` carrying
    // the worker's body ("no registered plan with trade_id …"), so `?` surfaces
    // it as a clean error rather than printing an empty table.
    let response = post_control(&args.common.endpoint, &body)?;
    // `plan show` is already a full dump; the pretty view just adds a header
    // per match. The raw YAML *is* the useful content, so default and --yaml
    // differ only in that header.
    if args.yaml {
        print_raw(&response);
        return Ok(());
    }
    print!("{}", format_plan_show(&args.trade_id, &response));
    Ok(())
}

/// `plan delete <trade_id>` — drop a registered plan and its engine state.
/// The inverse of `register`. Idempotent: the worker returns `ok` whether or
/// not a plan existed under that id, so re-running is safe. Prints the
/// worker's response verbatim.
fn run_plan_delete(args: PlanDeleteArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_plan_delete_intent(&args.trade_id, now, &suffix);
    let body = wrap_control(&intent, &key, now)?;
    let response = post_control(&args.common.endpoint, &body)?;
    print_raw(&response);
    Ok(())
}

/// Print a worker response verbatim, ensuring a trailing newline.
fn print_raw(response: &str) {
    print!("{response}");
    if !response.ends_with('\n') {
        println!();
    }
}

/// Render the `plan list` summaries as an aligned table. Each summary is a
/// generic YAML map (we don't link the CLI to the worker's `PlanSummary`
/// struct); missing fields render as `-`.
fn format_plan_list(plans: &[serde_yaml::Value]) -> String {
    if plans.is_empty() {
        return "no registered plans\n".to_string();
    }
    let field = |p: &serde_yaml::Value, k: &str| -> String {
        match p.get(k) {
            Some(serde_yaml::Value::Sequence(s)) => s
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
                .join(","),
            Some(v) => yaml_scalar(v),
            None => "-".to_string(),
        }
    };
    // `ARCHIVED` is the terminated-plan marker: the worker stamps `archived_at`
    // only on rows it read from the archive keyspace, so a live plan renders `-`
    // there. Surfaced by `plan list --include-all`.
    let rows: Vec<[String; 8]> = plans
        .iter()
        .map(|p| {
            [
                field(p, "trade_id"),
                field(p, "account"),
                field(p, "instrument"),
                field(p, "shadow"),
                field(p, "phase"),
                field(p, "rules"),
                field(p, "fired"),
                field(p, "archived_at"),
            ]
        })
        .collect();
    let headers = [
        "TRADE_ID",
        "ACCOUNT",
        "INSTRUMENT",
        "SHADOW",
        "PHASE",
        "RULES",
        "FIRED",
        "ARCHIVED",
    ];
    render_table(&headers, &rows)
}

/// One scalar YAML value as a plain string (no quotes), `-` for null.
fn yaml_scalar(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::Null => "-".to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::String(s) => s.clone(),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

/// Render a fixed-column table with each column widened to its longest cell.
fn render_table<const N: usize>(headers: &[&str; N], rows: &[[String; N]]) -> String {
    let mut widths = headers.map(str::len);
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let line = |cells: &[String; N]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let header_row: [String; N] = std::array::from_fn(|i| headers[i].to_string());
    let mut out = format!("{}\n", line(&header_row));
    for row in rows {
        out.push_str(&format!("{}\n", line(row)));
    }
    out
}

/// Format the `plan show` response: a short header per matched plan, then the
/// worker's YAML for that match. The worker returns a YAML sequence (one entry
/// per account scope that has the trade_id); we just label and pass through.
fn format_plan_show(trade_id: &str, response: &str) -> String {
    match serde_yaml::from_str::<Vec<serde_yaml::Value>>(response) {
        Ok(matches) if !matches.is_empty() => {
            let mut out = format!(
                "plan {trade_id} — {} match{}\n\n",
                matches.len(),
                if matches.len() == 1 { "" } else { "es" },
            );
            for m in &matches {
                out.push_str(&serde_yaml::to_string(m).unwrap_or_default());
                out.push('\n');
            }
            out
        }
        // Empty sequence, a 404 error body, or an unexpected shape: pass through.
        _ => {
            let mut out = response.to_string();
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out
        }
    }
}

fn run_market_info(args: MarketInfoCmdArgs) -> Result<()> {
    let key = load_key(&args.common.key_file)?;
    // Market-info is TradeNation-only, so always canonicalize against the
    // TN catalog (unless --force) — lets the operator type US30 / EUR_USD
    // and have it resolved to the broker's MarketName before sending.
    let instrument =
        canonicalize_instrument_or_force(BrokerKindArg::TradeNation, &args.instrument, args.force)?;
    let now = Utc::now();
    let suffix = fresh_suffix()?;
    let intent = build_market_info_intent(&instrument, now, &suffix);
    let body = wrap_control(&intent, &key, now)?;
    let response = post_control(&args.common.endpoint, &body)?;
    // The worker returns the MarketInfo serialised as YAML. Pretty-print
    // it Brisbane-first; if it can't be parsed (unexpected shape / error
    // body), fall back to the raw pass-through so nothing is hidden.
    match serde_yaml::from_str::<tradenation_api::MarketInfo>(&response) {
        Ok(info) => print!("{}", format_market_info(&info)),
        Err(_) => {
            print!("{response}");
            if !response.ends_with('\n') {
                println!();
            }
        }
    }
    Ok(())
}

/// Format a [`MarketInfo`](tradenation_api::MarketInfo) for the operator,
/// **Brisbane time first** (their zone) with London alongside for clarity.
///
/// Trading hours come from the broker in London local; each parsed range
/// carries both. When the broker returned non-range text (e.g.
/// `"24 Hours"`), `ranges` is empty and we print `raw_london` verbatim.
fn format_market_info(info: &tradenation_api::MarketInfo) -> String {
    let mut out = format!("{}\n", info.name);
    out.push_str("\ntrading hours (Brisbane / London):\n");
    if info.trade_session.ranges.is_empty() {
        // Non-range text like "24 Hours" — show the broker's raw string.
        out.push_str(&format!("  {}\n", info.trade_session.raw_london));
    } else {
        for r in &info.trade_session.ranges {
            out.push_str(&format!(
                "  {} - {}   (London {} - {})\n",
                r.open_brisbane, r.close_brisbane, r.open_london, r.close_london,
            ));
        }
    }
    out.push_str(&format!("\nspread:            {}\n", info.spread));
    out.push_str(&format!("margin:            {}\n", info.margin));
    out.push_str(&format!("stop orders:       {}\n", info.allow_stop_orders));
    out.push_str(&format!(
        "guaranteed stop:   {} (distance {}, charge {})\n",
        info.allow_guaranteed_stop, info.gsl_distance, info.gsl_charge,
    ));
    out.push_str(&format!("min/max stake:     {}\n", info.min_max_stake));
    out.push_str(&format!(
        "contract:          {} (rolling: {})\n",
        info.contract_month, info.is_rolling_market,
    ));
    out.push_str(&format!(
        "expiry:            {} (London {})\n",
        info.expiry_date_brisbane, info.expiry_date_london,
    ));
    out
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
    let cache = load_cache(false, None, None).ok();
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
    // No account context on these admin commands — fall back to the
    // default-demo login. The catalog is account-agnostic so this is
    // safe; if the local default demo is missing the operator gets a
    // clear error pointing at the local store.
    match validate_instrument(broker_kind, None, instrument)? {
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
        &args.trade_id,
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
    let intent = build_clear_prep_intent(
        &instrument,
        &args.step,
        args.account.as_deref(),
        now,
        &suffix,
    );
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
    let intent = build_clear_veto_intent(
        &instrument,
        &args.trade_id,
        &args.name,
        args.account.as_deref(),
        now,
        &suffix,
    );
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

/// Print the union of operator history and local TN store names, one
/// per line. No auth, no network — designed for shell tab-completion
/// hooks that run on every TAB. Errors are swallowed: an empty list
/// is the right behavior when there's nothing cached yet.
fn run_account_names() -> Result<()> {
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in trade_control_cli::load_history().accounts {
        names.insert(entry.name);
    }
    if let Ok(local) = tradenation_api::accounts::list_accounts() {
        for (name, _) in local {
            names.insert(name);
        }
    }
    for n in names {
        println!("{n}");
    }
    Ok(())
}

fn run_account(sub: AccountCmd) -> Result<()> {
    match sub {
        AccountCmd::List(args) => run_account_list(args),
        AccountCmd::Add(args) => run_account_add(args),
        AccountCmd::Delete(args) => run_account_delete(args),
        AccountCmd::Test(args) => run_account_test(args),
        AccountCmd::Names => run_account_names(),
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

fn run_adopt_trade(args: AdoptTradeArgs) -> Result<()> {
    let admin_key = load_admin_key(&args.common.admin_key_file)?;
    let body = AdoptBody {
        account: args.account.clone(),
        trade_id: args.trade_id,
        instrument: args.instrument,
        direction: args.direction.into(),
        broker_order_id: args.order_id,
        broker_trade_id: args.position_id,
        stop_loss_price: args.stop_loss,
    };
    let resp = adopt_trade(&args.common.endpoint, &admin_key, &body)?;
    // The verb hits the worker for a state mutation against a known
    // account, so cache the account name for shell completion the
    // same way `account test` does.
    record_account_use(&args.account);
    print!("{resp}");
    if !resp.ends_with('\n') {
        println!();
    }
    Ok(())
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
        delete_secret(&binding, &args.common.worker_name)?;
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

    // Enforce the intended order: create the broker account first
    // (`tradenation account create <name>`), then register it here.
    // Catches drift before we upload metadata to the worker — and lets
    // us pull credentials from the local store instead of re-prompting.
    if broker == BrokerKind::TradeNation {
        require_local_tn_account(&args.name)?;
    }

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
        // Default path: pull both username and password from the local
        // TN store. Removes a class of "operator re-typed the password
        // wrong" bugs that silently uploaded bad creds to Cloudflare.
        // `--username` is an explicit override — operator wants to
        // register a different identity than what's stored locally.
        let (username, password) = match args.username {
            Some(u) => {
                let password = Password::with_theme(&theme)
                    .with_prompt("TradeNation password")
                    .interact()
                    .map_err(|e| eyre!("read password: {e}"))?;
                (u, password)
            }
            None => {
                let acct = tradenation_api::accounts::get_account(&args.name)
                    .map_err(|e| eyre!("reading local TN store for '{}': {e}", args.name))?;
                eprintln!(
                    "using local TN store credentials for '{}' (username={})",
                    args.name, acct.username,
                );
                (acct.username, acct.password)
            }
        };
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
            put_secret(&binding, &args.common.worker_name, &creds)?;
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

    #[test]
    fn shell_from_path_takes_basename() {
        assert_eq!(shell_from_path("/usr/bin/zsh").unwrap(), Shell::Zsh);
        assert_eq!(shell_from_path("/bin/bash").unwrap(), Shell::Bash);
        assert_eq!(shell_from_path("fish").unwrap(), Shell::Fish);
    }

    /// A MarketInfo fixture, deserialised the same way the CLI deserialises
    /// the worker's response — so this also exercises the wire→struct path.
    fn market_info_fixture(ranges_yaml: &str, raw_london: &str) -> tradenation_api::MarketInfo {
        let yaml = format!(
            "name: Wall Street 30
trade_session:
  raw_london: \"{raw_london}\"
  ranges:
{ranges_yaml}
spread: \"4\"
spread_type: S
margin: 0.5%
bet_per: \"1\"
min_max_stake: USD,0.1,1000000
allow_stop_orders: \"Yes\"
allow_guaranteed_stop: \"Yes\"
gsl_charge: \"3\"
gsl_distance: 2%
contract_month: Rolling
last_trading: N/A
basis_expiry: rollover
expiry_date_london: \"-\"
expiry_date_brisbane: \"-\"
is_rolling_market: true
reset_time: 10:00 PM
"
        );
        serde_yaml::from_str(&yaml).expect("fixture deserialises")
    }

    #[test]
    fn format_market_info_is_brisbane_first_with_london_alongside() {
        let info = market_info_fixture(
            "    - open_london: \"23:00\"\n      close_london: \"21:00\"\n      \
             open_brisbane: \"09:00 (+1d)\"\n      close_brisbane: \"07:00 (+1d)\"",
            "23:00 - 21:00",
        );
        let out = format_market_info(&info);
        // Brisbane leads the range line; London is shown alongside.
        assert!(out.contains("09:00 (+1d) - 07:00 (+1d)   (London 23:00 - 21:00)"));
        assert!(out.contains("trading hours (Brisbane / London):"));
        assert!(out.contains("Wall Street 30"));
    }

    #[test]
    fn format_market_info_falls_back_to_raw_when_no_ranges() {
        // Broker returned non-range text like "24 Hours" — ranges empty.
        let info = market_info_fixture("    []", "24 Hours");
        let out = format_market_info(&info);
        assert!(out.contains("  24 Hours\n"));
        // No fabricated range line (those carry " - " between two clock
        // times alongside a "(London … - …)" suffix).
        assert!(!out.contains("(London 0"));
    }

    #[test]
    fn shell_from_path_rejects_unknown_shell() {
        assert!(shell_from_path("/usr/bin/nu").is_err());
    }

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

    #[test]
    fn plan_list_table_aligns_and_fills_missing() {
        // Two plans: one fully populated + shadow, one registered-but-not-yet-
        // ticked (no state → no phase, empty fired). A null account renders `-`.
        let yaml = "\
- trade_id: eurusd-hs-7
  account: reversals
  instrument: EUR_USD
  shadow: true
  rules: 6
  phase: await_entry
  fired: [03-prep-break-and-close, 04-prep-retest]
- trade_id: usdjpy-mw-3
  instrument: USD_JPY
  shadow: false
  rules: 4
";
        let plans: Vec<serde_yaml::Value> = serde_yaml::from_str(yaml).unwrap();
        let table = format_plan_list(&plans);
        let lines: Vec<&str> = table.lines().collect();
        assert!(lines[0].starts_with("TRADE_ID"), "header: {}", lines[0]);
        // Fully-populated row.
        assert!(lines[1].contains("eurusd-hs-7"));
        assert!(lines[1].contains("reversals"));
        assert!(lines[1].contains("true"));
        assert!(lines[1].contains("await_entry"));
        assert!(lines[1].contains("03-prep-break-and-close,04-prep-retest"));
        // Not-yet-ticked row: missing account/phase/fired render as `-`.
        assert!(lines[2].contains("usdjpy-mw-3"));
        assert!(lines[2].contains("USD_JPY"));
        // The ACCOUNT and PHASE columns are `-` for this row.
        assert!(
            lines[2].contains('-'),
            "expected `-` placeholder: {}",
            lines[2]
        );
    }

    #[test]
    fn plan_list_empty_is_friendly() {
        assert_eq!(format_plan_list(&[]), "no registered plans\n");
    }

    #[test]
    fn plan_show_labels_each_match() {
        let yaml = "\
- account: reversals
  plan:
    trade_id: eurusd-hs-7
    instrument: EUR_USD
  state:
    phase: await_entry
";
        let out = format_plan_show("eurusd-hs-7", yaml);
        assert!(
            out.starts_with("plan eurusd-hs-7 — 1 match\n"),
            "got: {out}"
        );
        assert!(out.contains("trade_id: eurusd-hs-7"));
        assert!(out.contains("phase: await_entry"));
    }
}
