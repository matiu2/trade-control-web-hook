//! End-to-end orchestration: read TV chart → classify drawings →
//! build trade + pause + news + calendar bundles → create alerts.
//!
//! Port of `tv_arm_hs.py::main()` (lines ~1548–2006). The library
//! calls into `trade-control-cli` directly rather than shelling out
//! to the binary (faster startup + structured errors).
//!
//! Two-pass flow for blackout/news/calendar bars:
//!
//! 1. Classify the chart drawings. If the operator has already drawn
//!    `blackout-*` or `news-*` pairs, use those as-is.
//! 2. Otherwise (and `--skip-calendar-bars` is not set), fetch this
//!    week's forex-factory events for the chart's symbol, draw a
//!    vertical line per window edge via tv-mcp, then re-classify.
//!    From that point on the auto-drawn lines look identical to
//!    human-drawn ones.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use color_eyre::eyre::{Context, Result, eyre};
use tracing::{info, warn};
use trade_control_cli as cli;
use trade_control_conventions::{Broker, Direction, split_symbol};
use trade_control_core::sig::KEY_LEN;

use crate::alert_spec::{AlertPayload, CalendarWindow, DispatchContext, build_alert_spec};
use crate::args::Args;
use crate::create_alerts::create_alerts;
use crate::geometry::tp_price_from_fib;
use crate::manifest::{CalendarBundle, discover_calendar_bundles};
use crate::roles::{Roles, classify};
use crate::timeframe::infer_calendar_timeframe;
use trading_view::drawings::Drawing;
use trading_view::mcp::TvMcp;

/// Output root for built bundles. Matches the Python `ARM_OUT_ROOT`
/// so a side-by-side run reuses the same paths.
const ARM_OUT_ROOT: &str = "/tmp/trade-control-arm";

/// Drive the full flow. Returns process exit code; non-zero means a
/// step failed (chart classification, build-trade, etc.). All errors
/// are logged before the function returns.
pub fn run(args: Args) -> Result<i32> {
    // 1. Read chart state + decide broker / instrument.
    let mcp = TvMcp::new(
        args.tv_mcp_root
            .clone()
            .unwrap_or_else(|| PathBuf::from(trading_view::mcp::DEFAULT_TV_MCP_ROOT)),
    );
    let state = mcp.get_state().wrap_err("read TV chart state")?;
    let (_exchange, raw_sym) = split_symbol(&state.symbol);
    let raw_sym = raw_sym.to_string();
    let broker = resolve_broker(&args, &state.symbol)?;
    // Resolve through the instrument-lookup catalog: this both
    // validates that the asset is listed on the chosen broker and
    // gives us the broker-canonical symbol (`EUR/USD` for TN,
    // `EUR_USD` for OANDA, `Switzerland 20` for SMI on TN, etc.).
    // Hard errors if the chart's symbol isn't in the catalog.
    let resolved = crate::instrument_resolution::resolve_for_broker(&state.symbol, broker)?;
    let instrument = resolved.broker_symbol.clone();

    info!(
        chart = %state.symbol,
        asset_id = %resolved.asset.id,
        resolution = %state.resolution,
        broker = broker.as_str(),
        instrument = %instrument,
        "arming reversal setup"
    );

    // 2. First-pass classify. If no blackout/news pairs are present
    //    and the operator didn't opt out, auto-draw from
    //    forex-factory calendar.
    let mut drawings = mcp.list_drawings().wrap_err("list TV drawings")?;
    let mut roles = classify(&mcp, &drawings)?;

    let should_auto_draw =
        !args.skip_calendar_bars && roles.blackout_pairs.is_empty() && roles.news_pairs.is_empty();
    if should_auto_draw {
        if let Err(e) = auto_draw_calendar_lines(&mcp, &state.resolution, &resolved) {
            warn!(error = ?e, "calendar auto-draw failed; continuing with chart as-is");
        } else {
            drawings = mcp.list_drawings().wrap_err("re-list TV drawings")?;
            roles = classify(&mcp, &drawings)?;
        }
    }

    // 3. Validate required drawings are present.
    if let Err(msg) = check_required(&roles, &args) {
        eprintln!("ERROR: {msg}");
        return Ok(1);
    }

    // 4. Direction + TP + expiry from the classified drawings.
    let inv_label = roles.invalidation_label.clone().unwrap_or_default();
    let direction = Direction::from_invalidation_label(&inv_label)
        .ok_or_else(|| eyre!("invalid invalidation label {inv_label:?}"))?;
    let tp_fib = roles
        .tp_fib
        .as_ref()
        .ok_or_else(|| eyre!("missing tp_fib (already checked in step 3)"))?;
    let tp = tp_price_from_fib(&tp_fib.prices(), direction);
    let trade_expiry_d = roles
        .trade_expiry
        .as_ref()
        .ok_or_else(|| eyre!("missing trade_expiry (already checked in step 3)"))?;
    let expiry_unix = trade_expiry_d
        .points
        .first()
        .ok_or_else(|| eyre!("trade_expiry has no points"))?
        .time;
    let expiry = Utc
        .timestamp_opt(expiry_unix, 0)
        .single()
        .ok_or_else(|| eyre!("invalid trade_expiry timestamp {expiry_unix}"))?;

    info!(
        direction = direction.as_str(),
        tp = %format!("{tp:.5}"),
        trade_expiry = %expiry.to_rfc3339(),
        "trade geometry resolved"
    );

    // 5. Build the trade bundle.
    let key = read_key()?;
    let account = resolve_account(&args, broker);
    let out_dir = arm_out_dir(&raw_sym)?;
    let now = Utc::now();
    let trade_spec = build_trade_spec(
        &args,
        &instrument,
        &account,
        broker,
        direction,
        expiry,
        tp,
        &roles,
    );
    let built_trade = cli::build_trade_from_spec(trade_spec, now).wrap_err("build trade bundle")?;
    let trade_id = built_trade.trade_id.clone();
    cli::write_trade(&built_trade, &key, &out_dir).wrap_err("write trade bundle")?;
    info!(
        trade_id = %trade_id,
        out_dir = %out_dir.display(),
        alerts = built_trade.alerts.len(),
        "trade bundle written"
    );

    // 6. Pause bundles per blackout pair.
    let pause_bundles = build_pause_bundles(
        &roles,
        &trade_id,
        &instrument,
        &account,
        broker,
        &out_dir,
        &key,
        now,
    )?;

    // 7. News bundles per news pair.
    let news_bundles = build_news_bundles(
        &roles,
        &trade_id,
        &instrument,
        &account,
        broker,
        &out_dir,
        &key,
        now,
    )?;

    // 8. Calendar bundles (skipped if auto-draw already handled it,
    //    or if --skip-calendar-bars was passed). When auto-draw ran,
    //    the operator now has blackout-/news-pairs on the chart, so
    //    we skip the cli's calendar-bars step to avoid double-arming.
    let calendar_bundles = if should_auto_draw || args.skip_calendar_bars {
        Vec::new()
    } else {
        discover_or_fetch_calendar_bundles(
            &args,
            &state,
            &trade_id,
            &instrument,
            &account,
            broker,
            &out_dir,
            &key,
            now,
        )?
    };

    // 9. Bail out before POSTing if --create-alerts wasn't set.
    if !args.create_alerts {
        info!("--create-alerts not set; signed bundle on disk, no TV POSTs");
        return Ok(0);
    }

    // 10. Build payloads + POST.
    let payloads = build_all_payloads(
        &built_trade,
        &out_dir,
        direction,
        &roles,
        &pause_bundles,
        &news_bundles,
        &calendar_bundles,
        &trade_id,
    )?;
    if payloads.is_empty() {
        info!("no payloads to POST");
        return Ok(0);
    }
    let results = create_alerts(&payloads, mcp.root()).wrap_err("create alerts via tv-mcp")?;
    for r in &results {
        match (&r.status, &r.error) {
            (Some(status), _) => {
                let body = r.body.as_deref().unwrap_or("");
                let body_head = body.chars().take(200).collect::<String>();
                info!(name = ?r.name, status, body = %body_head, "alert POSTed");
            }
            (None, Some(err)) => {
                warn!(name = ?r.name, error = %err, "alert FAILED");
            }
            (None, None) => {
                warn!(name = ?r.name, "alert returned with no status or error");
            }
        }
    }
    Ok(0)
}

/// Resolve `--broker` > `TRADE_CONTROL_BROKER` env > chart exchange.
fn resolve_broker(args: &Args, full_symbol: &str) -> Result<Broker> {
    if let Some(arg) = args.broker {
        return Ok(arg.into_conventions());
    }
    if let Ok(env_val) = env::var("TRADE_CONTROL_BROKER") {
        let trimmed = env_val.trim();
        if !trimmed.is_empty() {
            if let Some(b) = Broker::from_wire(trimmed) {
                return Ok(b);
            }
            return Err(eyre!("unsupported TRADE_CONTROL_BROKER {trimmed:?}"));
        }
    }
    let (exchange, _) = split_symbol(full_symbol);
    Ok(exchange
        .and_then(Broker::from_exchange)
        .unwrap_or(Broker::Oanda))
}

/// `--account-id` > `TRADE_CONTROL_ACCOUNT` env > per-broker default.
fn resolve_account(args: &Args, broker: Broker) -> String {
    if let Some(a) = &args.account_id {
        return a.clone();
    }
    if let Ok(env_val) = env::var("TRADE_CONTROL_ACCOUNT") {
        let trimmed = env_val.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    broker.default_account_index().to_string()
}

/// Validate the chart has every drawing the bundle will need.
/// Mirrors `tv_arm_hs.py:1614-1629`.
fn check_required(roles: &Roles, args: &Args) -> std::result::Result<(), String> {
    let mut missing = Vec::new();
    if roles.invalidation.is_none() {
        missing.push("horizontal_line labeled 'too-high' or 'too-low'");
    }
    if roles.break_and_close.is_none() && !args.skip_break_and_close {
        missing.push("trend_line labeled 'neckline' (or 'break-and-close')");
    }
    if roles.retest.is_none() && !args.skip_retest {
        missing.push("trend_line labeled 'retest'");
    }
    if roles.tp_fib.is_none() {
        missing.push("fib_retracement (TP)");
    }
    if roles.trade_expiry.is_none() {
        missing.push("vertical_line labeled 'trade-expiry'");
    }
    if missing.is_empty() {
        return Ok(());
    }
    let mut msg = String::from("missing required drawings:\n");
    for m in missing {
        msg.push_str("  - ");
        msg.push_str(m);
        msg.push('\n');
    }
    Err(msg)
}

/// Load the signing key from `TRADE_CONTROL_KEY_FILE` env or the
/// default `~/.config/trade-control/key.hex`.
fn read_key() -> Result<[u8; KEY_LEN]> {
    let path = key_path_resolved()?;
    let hex_str =
        fs::read_to_string(&path).with_context(|| format!("read key file {}", path.display()))?;
    let bytes = hex::decode(hex_str.trim())
        .with_context(|| format!("decode key hex from {}", path.display()))?;
    if bytes.len() != KEY_LEN {
        return Err(eyre!(
            "key at {} is {} bytes, expected {}",
            path.display(),
            bytes.len(),
            KEY_LEN
        ));
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(key)
}

fn default_key_path() -> Result<PathBuf> {
    let home = env::var("HOME").map_err(|_| eyre!("HOME env not set"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("trade-control")
        .join("key.hex"))
}

/// Same precedence as [`read_key`] but returns the path instead of
/// the bytes — needed for `CalendarBarsArgs.key_file`.
fn key_path_resolved() -> Result<PathBuf> {
    if let Ok(env) = env::var("TRADE_CONTROL_KEY_FILE")
        && !env.trim().is_empty()
    {
        return Ok(PathBuf::from(env));
    }
    default_key_path()
}

fn arm_out_dir(raw_sym: &str) -> Result<PathBuf> {
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let dir = PathBuf::from(ARM_OUT_ROOT).join(format!("{raw_sym}-{today}"));
    fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    Ok(dir)
}

/// Assemble the `TradeSpec` from CLI args + classified roles.
#[allow(clippy::too_many_arguments)]
fn build_trade_spec(
    args: &Args,
    instrument: &str,
    account: &str,
    broker: Broker,
    direction: Direction,
    expiry: DateTime<Utc>,
    tp: f64,
    roles: &Roles,
) -> cli::TradeSpec {
    use cli::TradePattern;
    let pattern = match direction {
        Direction::Short => TradePattern::Hs,
        Direction::Long => TradePattern::Ihs,
    };
    let mut skip_preps = Vec::new();
    if args.skip_break_and_close {
        skip_preps.push("break-and-close".to_string());
    }
    if args.skip_retest {
        skip_preps.push("retest".to_string());
    }
    let mut spec = cli::TradeSpec {
        pattern,
        instrument: instrument.to_string(),
        account: account.to_string(),
        broker: broker_to_kind(broker),
        trade_expiry: expiry,
        risk_pct: args.risk_pct.unwrap_or(1.0),
        risk_amount: args.risk_amount,
        dry_run: args.broker_dry_run,
        max_retries: args.max_retries.unwrap_or(0),
        skip_preps,
        entry_offset_pips: None,
        sl_offset_pips: None,
        sl_anchor: None,
        tp_price: round5(tp),
        entry_deadline_pct: 80,
        allow_entry: args.entry_filter_script.clone(),
        entry_mode: if args.entry_market {
            cli::EntryMode::Market
        } else {
            cli::EntryMode::Stop
        },
        needs_golden: args.require_golden,
        close_on_news: !roles.news_pairs.is_empty(),
        sr_reversal_ranges: build_sr_ranges(roles, args.reversal_band_pct),
    };
    if args.sl_from_recent {
        spec.sl_anchor = Some(match direction {
            Direction::Short => cli::PriceAnchor::RecentHigh,
            Direction::Long => cli::PriceAnchor::RecentLow,
        });
    }
    spec
}

fn broker_to_kind(b: Broker) -> cli::BrokerKind {
    match b {
        Broker::Oanda => cli::BrokerKind::Oanda,
        Broker::TradeNation => cli::BrokerKind::TradeNation,
    }
}

fn build_sr_ranges(roles: &Roles, band_pct: f64) -> Vec<[f64; 2]> {
    let pct = band_pct / 100.0;
    roles
        .sr_levels
        .iter()
        .filter_map(|d| d.points.first().map(|p| p.price))
        .map(|price| [round5(price * (1.0 - pct)), round5(price * (1.0 + pct))])
        .collect()
}

fn round5(v: f64) -> f64 {
    (v * 1e5).round() / 1e5
}

/// In-memory representation of one built pause / news bundle so the
/// payload loop downstream can iterate without re-reading disk.
struct PauseBundle {
    start: Drawing,
    end: Drawing,
    built: cli::BuiltPause,
    out_dir: PathBuf,
}

struct NewsBundle {
    start: Drawing,
    end: Drawing,
    built: cli::BuiltNews,
    out_dir: PathBuf,
}

#[allow(clippy::too_many_arguments)]
fn build_pause_bundles(
    roles: &Roles,
    trade_id: &str,
    instrument: &str,
    account: &str,
    broker: Broker,
    out_dir: &Path,
    key: &[u8; KEY_LEN],
    now: DateTime<Utc>,
) -> Result<Vec<PauseBundle>> {
    let mut bundles = Vec::new();
    if roles.blackout_pairs.is_empty() {
        return Ok(bundles);
    }
    if trade_id.is_empty() {
        return Err(eyre!(
            "have blackout pairs but trade has no trade_id; refusing to arm"
        ));
    }
    for (i, (start_d, end_d)) in roles.blackout_pairs.iter().enumerate() {
        let pair_idx = i + 1;
        let start_iso = utc_iso(start_d.anchor_time_seconds())?;
        let end_iso = utc_iso(end_d.anchor_time_seconds())?;
        let pause_dir = out_dir.join(format!("pause-{pair_idx}"));
        fs::create_dir_all(&pause_dir).with_context(|| format!("mkdir {}", pause_dir.display()))?;
        let spec = cli::PauseSpec {
            trade_id: trade_id.to_string(),
            blackout_id: None,
            instrument: instrument.to_string(),
            account: account.to_string(),
            broker: broker_to_kind(broker),
            start_time: parse_iso(&start_iso)?,
            end_time: parse_iso(&end_iso)?,
            reason: Some(format!("news:{instrument}-{start_iso}")),
        };
        let built = cli::build_pause_from_spec(spec, now)
            .with_context(|| format!("build pause #{pair_idx}"))?;
        cli::write_pause(&built, key, &pause_dir)
            .with_context(|| format!("write pause #{pair_idx}"))?;
        bundles.push(PauseBundle {
            start: start_d.clone(),
            end: end_d.clone(),
            built,
            out_dir: pause_dir,
        });
    }
    info!(count = bundles.len(), "pause bundles built");
    Ok(bundles)
}

#[allow(clippy::too_many_arguments)]
fn build_news_bundles(
    roles: &Roles,
    trade_id: &str,
    instrument: &str,
    account: &str,
    broker: Broker,
    out_dir: &Path,
    key: &[u8; KEY_LEN],
    now: DateTime<Utc>,
) -> Result<Vec<NewsBundle>> {
    let mut bundles = Vec::new();
    if roles.news_pairs.is_empty() {
        return Ok(bundles);
    }
    if trade_id.is_empty() {
        return Err(eyre!(
            "have news pairs but trade has no trade_id; refusing to arm"
        ));
    }
    for (i, (start_d, end_d)) in roles.news_pairs.iter().enumerate() {
        let pair_idx = i + 1;
        let start_iso = utc_iso(start_d.anchor_time_seconds())?;
        let end_iso = utc_iso(end_d.anchor_time_seconds())?;
        let news_dir = out_dir.join(format!("news-{pair_idx}"));
        fs::create_dir_all(&news_dir).with_context(|| format!("mkdir {}", news_dir.display()))?;
        let spec = cli::NewsSpec {
            trade_id: trade_id.to_string(),
            news_id: None,
            instrument: instrument.to_string(),
            account: account.to_string(),
            broker: broker_to_kind(broker),
            start_time: parse_iso(&start_iso)?,
            end_time: parse_iso(&end_iso)?,
            reason: Some(format!("news:{instrument}-{start_iso}")),
        };
        let built = cli::build_news_from_spec(spec, now)
            .with_context(|| format!("build news #{pair_idx}"))?;
        cli::write_news(&built, key, &news_dir)
            .with_context(|| format!("write news #{pair_idx}"))?;
        bundles.push(NewsBundle {
            start: start_d.clone(),
            end: end_d.clone(),
            built,
            out_dir: news_dir,
        });
    }
    info!(count = bundles.len(), "news bundles built");
    Ok(bundles)
}

/// Auto-draw vertical lines on the chart from this week's
/// forex-factory events. Used when the operator hasn't drawn any
/// blackout/news pairs themselves.
fn auto_draw_calendar_lines(
    mcp: &TvMcp,
    resolution: &str,
    resolved: &crate::instrument_resolution::ResolvedInstrument,
) -> Result<()> {
    let timeframe = infer_calendar_timeframe(resolution).ok_or_else(|| {
        eyre!("chart resolution {resolution:?} is below 15m; calendar bars skipped")
    })?;
    let now = Utc::now();
    // Synthesise the tcm Instrument straight from the catalog Asset
    // so non-FX assets (SMI, gold, indices) get correct news-currency
    // exposure without the FX-only cli::parse_instrument path.
    let instrument_parsed =
        crate::instrument_resolution::synthesize_calendar_instrument(resolved.asset);
    let runtime = tokio::runtime::Runtime::new().context("starting tokio runtime")?;
    let events = runtime
        .block_on(cli::fetch_week_events(now))
        .wrap_err("fetch_week_events")?;
    let inputs = cli::PlanInputs {
        // trade_id isn't used for the line geometry — empty string is fine.
        trade_id: String::new(),
        instrument: resolved.broker_symbol.clone(),
        account: String::new(),
        broker: cli::BrokerKind::Oanda,
    };
    let plan = cli::plan_calendar_bars(&events, &instrument_parsed, timeframe.into(), now, &inputs)
        .wrap_err("plan_calendar_bars")?;
    if plan.rows.is_empty() {
        info!("no calendar events in window — nothing to auto-draw");
        return Ok(());
    }
    // For each event, draw two pause vertical lines and two news
    // vertical lines (start + end of each window). The operator can
    // re-run after editing the chart to fine-tune.
    for row in &plan.rows {
        draw_pair_lines(
            mcp,
            &row.pause_spec.start_time,
            &row.pause_spec.end_time,
            "pause",
            "resume",
        )?;
        draw_pair_lines(
            mcp,
            &row.news_spec.start_time,
            &row.news_spec.end_time,
            "news-start",
            "news-end",
        )?;
    }
    info!(events = plan.rows.len(), "calendar lines auto-drawn");
    Ok(())
}

fn draw_pair_lines(
    mcp: &TvMcp,
    start: &DateTime<Utc>,
    end: &DateTime<Utc>,
    start_label: &str,
    end_label: &str,
) -> Result<()> {
    let s = mcp.draw_vertical_line(start.timestamp(), 1.0, start_label)?;
    let e = mcp.draw_vertical_line(end.timestamp(), 1.0, end_label)?;
    if !s.success {
        return Err(eyre!(
            "tv-mcp draw {start_label} failed: {:?}",
            s.error.as_deref().unwrap_or("(no error message)")
        ));
    }
    if !e.success {
        return Err(eyre!(
            "tv-mcp draw {end_label} failed: {:?}",
            e.error.as_deref().unwrap_or("(no error message)")
        ));
    }
    Ok(())
}

/// Run the calendar-bars CLI and discover the resulting bundles.
#[allow(clippy::too_many_arguments)]
fn discover_or_fetch_calendar_bundles(
    args: &Args,
    state: &trading_view::drawings::ChartState,
    trade_id: &str,
    instrument: &str,
    account: &str,
    broker: Broker,
    out_dir: &Path,
    key: &[u8; KEY_LEN],
    now: DateTime<Utc>,
) -> Result<Vec<CalendarBundle>> {
    if args.skip_calendar_bars {
        return Ok(Vec::new());
    }
    let timeframe = match infer_calendar_timeframe(&state.resolution) {
        Some(t) => t,
        None => {
            info!(resolution = %state.resolution, "below 15m — skipping calendar-bars");
            return Ok(Vec::new());
        }
    };
    let cli_broker = match broker {
        Broker::Oanda => cli::CalendarBrokerArg::Oanda,
        Broker::TradeNation => cli::CalendarBrokerArg::TradeNation,
    };
    let key_path = key_path_resolved()?;
    let cb_args = cli::CalendarBarsArgs {
        trade_id: trade_id.to_string(),
        instrument: instrument.to_string(),
        account: account.to_string(),
        broker: cli_broker,
        timeframe,
        key_file: key_path,
        output_dir: Some(out_dir.join("calendar-bars").join(trade_id)),
        dry_run: false,
    };
    if let Err(e) = cli::run_calendar_bars(cb_args, *key, now) {
        warn!(error = ?e, "calendar-bars failed; continuing without it");
        return Ok(Vec::new());
    }
    let bundles = discover_calendar_bundles(out_dir, trade_id)?;
    info!(count = bundles.len(), "calendar bundles discovered");
    Ok(bundles)
}

/// Walk every alert in every bundle and build the payload list.
#[allow(clippy::too_many_arguments)]
fn build_all_payloads(
    built_trade: &cli::BuiltTrade,
    out_dir: &Path,
    direction: Direction,
    roles: &Roles,
    pause_bundles: &[PauseBundle],
    news_bundles: &[NewsBundle],
    calendar_bundles: &[CalendarBundle],
    trade_id: &str,
) -> Result<Vec<AlertPayload>> {
    let mut payloads = Vec::new();
    // 1. Main trade alerts.
    for alert in &built_trade.alerts {
        let file = format!("{}.yaml", alert.basename);
        let ctx = DispatchContext::default();
        if let Some(mut p) = build_alert_spec(&file, direction, roles, &ctx)? {
            stamp_tv_name(&mut p, trade_id);
            payloads.push(p);
        }
    }
    let _ = out_dir; // YAMLs are on-disk; the JS reads them via `message` field.

    // 2. Pause bundles.
    for bundle in pause_bundles {
        let ctx = DispatchContext {
            blackout_pair: Some((&bundle.start, &bundle.end)),
            ..Default::default()
        };
        for alert in &bundle.built.alerts {
            let file = format!("{}.yaml", alert.basename);
            if let Some(mut p) = build_alert_spec(&file, direction, roles, &ctx)? {
                stamp_tv_name(&mut p, trade_id);
                payloads.push(p);
            }
        }
    }
    // 3. News bundles.
    for bundle in news_bundles {
        let ctx = DispatchContext {
            news_pair: Some((&bundle.start, &bundle.end)),
            ..Default::default()
        };
        for alert in &bundle.built.alerts {
            let file = format!("{}.yaml", alert.basename);
            if let Some(mut p) = build_alert_spec(&file, direction, roles, &ctx)? {
                stamp_tv_name(&mut p, trade_id);
                payloads.push(p);
            }
        }
    }
    // 4. Calendar bundles.
    for bundle in calendar_bundles {
        let ctx = DispatchContext {
            calendar_window: Some(CalendarWindow {
                start_iso: bundle.start_iso.clone(),
                end_iso: bundle.end_iso.clone(),
            }),
            ..Default::default()
        };
        for entry in &bundle.manifest.alerts {
            if let Some(mut p) = build_alert_spec(&entry.file, direction, roles, &ctx)? {
                stamp_tv_name(&mut p, trade_id);
                payloads.push(p);
            }
        }
    }
    Ok(payloads)
}

/// Mutate `tv_name` to `<trade_id>-<role_slug>` so all alerts sort
/// together in TV's alert list.
fn stamp_tv_name(payload: &mut AlertPayload, trade_id: &str) {
    if trade_id.is_empty() {
        return;
    }
    let stamp = |name: &mut String| {
        *name = format!("{trade_id}-{name}");
    };
    match payload {
        AlertPayload::Drawing { tv_name, .. }
        | AlertPayload::PriceValue { tv_name, .. }
        | AlertPayload::VertLineAt { tv_name, .. }
        | AlertPayload::PineAlertcondition { tv_name, .. } => stamp(tv_name),
    }
}

fn utc_iso(unix: i64) -> Result<String> {
    let dt = Utc
        .timestamp_opt(unix, 0)
        .single()
        .ok_or_else(|| eyre!("invalid epoch {unix}"))?;
    Ok(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

fn parse_iso(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| eyre!("parse_iso({s:?}): {e}"))
}

// `Drawing::anchor_time_seconds` shim — `TimedAnchor::anchor_time`
// already exists, but lives behind a trait import. Inline a fn
// here so the pipeline doesn't need to import the trait.
trait AnchorTimeShim {
    fn anchor_time_seconds(&self) -> i64;
}
impl AnchorTimeShim for Drawing {
    fn anchor_time_seconds(&self) -> i64 {
        use trading_view::pair_lines::TimedAnchor;
        self.anchor_time()
    }
}
