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
use tracing::{error, info, warn};
use trade_control_cli as cli;
use trade_control_conventions::{Broker, Direction, split_symbol};
use trade_control_core::sig::KEY_LEN;

use crate::alert_spec::{AlertPayload, CalendarWindow, DispatchContext, build_alert_spec};
use crate::args::Args;
use crate::create_alerts::create_alerts;
use crate::geometry::tp_price_from_fib;
use crate::manifest::{CalendarBundle, discover_calendar_bundles};
use crate::post_outcome::{Outcome, classify as classify_outcome};
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
    //
    // On a catalog miss we try to recover by asking tv-mcp for the
    // chart's symbol-info (`tv info`) — its `description` field
    // usually matches the broker's name for the asset (e.g. the
    // chart shows `GOOGL` but the catalog has `ALPHABET`). On a
    // successful recovery the user overlay is patched so future runs
    // resolve directly. If that also misses, we error with a
    // copy-pasteable TOML snippet built from the chart info.
    let resolved = resolve_with_recovery(&state.symbol, broker, &mcp)?;
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
    //
    //    The visible range scopes M/W path detection — only a path
    //    whose anchors all sit in the on-screen window counts (see
    //    `classify`). H&S drawings ignore it.
    let visible = mcp
        .get_range()
        .wrap_err("read TV visible range")?
        .visible_range;
    let view = (visible.from, visible.to);
    let mut drawings = mcp.list_drawings().wrap_err("list TV drawings")?;
    let mut roles = classify(&mcp, &drawings, view)?;

    let should_auto_draw =
        !args.skip_calendar_bars && roles.blackout_pairs.is_empty() && roles.news_pairs.is_empty();
    if should_auto_draw {
        // Read the trade-expiry drawing before auto-draw so we can widen
        // the calendar lookahead to cover the full trade lifetime, not
        // just the next ~9h H1+ buffer window. If the expiry drawing is
        // missing or unparseable, fall back to None — auto-draw will use
        // its default buffer window, and check_required (step 3) will
        // surface the missing drawing as a hard error shortly anyway.
        let expiry_hint = read_trade_expiry(&roles).ok();
        if let Err(e) = auto_draw_calendar_lines(&mcp, &state.resolution, &resolved, expiry_hint) {
            warn!(error = ?e, "calendar auto-draw failed; continuing with chart as-is");
        } else {
            drawings = mcp.list_drawings().wrap_err("re-list TV drawings")?;
            roles = classify(&mcp, &drawings, view)?;
        }
    }

    // 3. Validate required drawings are present.
    if let Err(msg) = check_required(&roles, &args) {
        eprintln!("ERROR: {msg}");
        return Ok(1);
    }
    // 3b. Validate prep-expiry lines against their preps. A future
    //     cutoff with no matching prep drawing would arm a setup that
    //     can never enter; a past cutoff is just a re-arm later in time.
    if let Err(msg) = check_prep_expiries(&roles, Utc::now()) {
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
    let expiry = read_trade_expiry(&roles)?;

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
    info!(
        sr_levels_classified = roles.sr_levels.len(),
        sr_reversal_ranges = trade_spec.sr_reversal_ranges.len(),
        news_pairs = roles.news_pairs.len(),
        blackout_pairs = roles.blackout_pairs.len(),
        "trade spec built — close-on-reversal coverage",
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
    let mut failures = 0usize;
    for r in &results {
        let outcome = classify_outcome(r);
        let body_head = r
            .body
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(400)
            .collect::<String>();
        match &outcome {
            Outcome::Ok => {
                info!(name = ?r.name, status = ?r.status, body = %body_head, "alert POSTed");
            }
            Outcome::TvError { errmsg, err_code } => {
                failures += 1;
                error!(
                    name = ?r.name,
                    status = ?r.status,
                    errmsg = errmsg.as_deref().unwrap_or(""),
                    err_code = err_code.as_deref().unwrap_or(""),
                    body = %body_head,
                    debug = ?r.debug,
                    "alert REJECTED by TradingView",
                );
            }
            Outcome::TransportError(err) => {
                failures += 1;
                error!(name = ?r.name, error = %err, "alert FAILED before POST");
            }
            Outcome::NoSignal => {
                failures += 1;
                warn!(name = ?r.name, "alert returned with no status or error");
            }
        }
    }
    if failures > 0 {
        error!(failures, total = results.len(), "some alerts did not arm");
        return Ok(1);
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

/// Resolve via the catalog; on miss, ask tv-mcp for the chart's
/// symbol-info and try to recover by patching the user overlay.
///
/// The recovery path covers the common case of a chart whose bare
/// TV symbol (e.g. `GOOGL`) doesn't match the catalog's id
/// (e.g. `ALPHABET`) — `tv info`'s `description` field carries the
/// broker's name, which usually does match. On success we patch the
/// overlay and re-resolve so the rest of the run sees the patched
/// asset. On failure we surface the original catalog-miss error,
/// supplemented with a copy-pasteable TOML snippet built from the
/// chart info.
fn resolve_with_recovery(
    tv_symbol: &str,
    broker: Broker,
    mcp: &TvMcp,
) -> Result<crate::instrument_resolution::ResolvedInstrument> {
    let first_err = match crate::instrument_resolution::resolve_for_broker(tv_symbol, broker) {
        Ok(resolved) => return Ok(resolved),
        Err(e) => e,
    };
    // Catalog miss — try the recovery path. If anything in here
    // fails, fall through to the original error so the operator sees
    // the actionable "add an overlay entry" hint.
    let info = match mcp.get_symbol_info() {
        Ok(info) => info,
        Err(e) => {
            warn!(error = ?e, "tv-mcp `info` call failed; can't auto-recover");
            return Err(first_err);
        }
    };
    let Some(patched) = crate::instrument_recovery::build_patched_asset(&info)? else {
        let snippet = crate::instrument_recovery::overlay_snippet_hint(&info);
        return Err(eyre!(
            "{first_err}\n\
             \n\
             Chart symbol-info: full_name={full_name:?}, description={desc:?}, \
             type={ty:?}.\n\
             Neither `description` nor `symbol` resolved either. Paste this \
             into your overlay (and edit the broker symbols / news_currencies \
             to match):\n\n{snippet}\n",
            full_name = info.full_name,
            desc = info.description,
            ty = info.asset_type,
        ));
    };
    let asset_id = patched.asset.id.clone();
    let overlay_path = match crate::instrument_recovery::save_patch(&patched) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = ?e, asset_id = %asset_id, "failed to persist overlay patch; aborting recovery");
            return Err(first_err);
        }
    };
    info!(
        chart_symbol = %tv_symbol,
        asset_id = %asset_id,
        overlay = %overlay_path.display(),
        "recovered unknown chart symbol via `tv info` and patched user overlay"
    );
    // The in-memory catalog is a LazyLock — already initialized
    // without our patch. We can't reload it cheaply, so resolve
    // directly off the patched asset we already built instead of
    // calling back into the catalog.
    let il_broker = match broker {
        Broker::Oanda => instrument_lookup::Broker::Oanda,
        Broker::TradeNation => instrument_lookup::Broker::TradeNation,
    };
    let broker_symbol = patched
        .asset
        .symbol_for(il_broker)
        .ok_or_else(|| {
            eyre!(
                "recovered asset {asset_id} via chart info, but it's not listed on {} \
                 (overlay patched at {} so the catalog now knows the TV symbol)",
                broker.as_str(),
                overlay_path.display(),
            )
        })?
        .to_string();
    // Leak the patched asset to satisfy the 'static reference in
    // ResolvedInstrument. This happens at most once per
    // tv-arm-invocation per unknown symbol, so the leak is bounded
    // and tiny.
    let leaked: &'static instrument_lookup::Asset = Box::leak(Box::new(patched.asset));
    Ok(crate::instrument_resolution::ResolvedInstrument {
        asset: leaked,
        broker_symbol,
    })
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

/// Canonical prep-step names that have a `<prep>-expiry` cutoff line on
/// the chart — fed into `cli::TradeSpec.prep_expiries` so the CLI emits
/// one `08-prep-expire-<step>` alert per line.
fn prep_expiry_steps(roles: &Roles) -> Vec<String> {
    roles
        .prep_expiries
        .iter()
        .map(|(step, _)| step.clone())
        .collect()
}

/// Validate each `<prep>-expiry` cutoff line against the prep it guards.
///
/// - **Future cutoff, no matching prep drawing** → hard error. The line
///   would block the prep before it could ever land, so the setup could
///   never enter — almost certainly the operator drew the cutoff but
///   forgot the neckline / retest trend line.
/// - **Past cutoff** → warn only. We're re-arming a setup later in time
///   (the cutoff already lapsed); the line is harmless context, not a
///   reason to abort.
///
/// `now` is injected so the rule is unit-testable without a clock.
fn check_prep_expiries(roles: &Roles, now: DateTime<Utc>) -> std::result::Result<(), String> {
    let now_unix = now.timestamp();
    let mut errors = Vec::new();
    for (step, drawing) in &roles.prep_expiries {
        let line_unix = drawing.anchor_time_seconds();
        let prep_present = match step.as_str() {
            trade_control_conventions::PREP_BREAK_AND_CLOSE => roles.break_and_close.is_some(),
            trade_control_conventions::PREP_RETEST => roles.retest.is_some(),
            // Unknown step shouldn't occur (classify only emits known
            // prep names), but treat it as "prep absent" defensively.
            _ => false,
        };
        if line_unix > now_unix {
            if !prep_present {
                errors.push(format!(
                    "  - '{step}-expiry' cutoff line is in the future but no '{step}' \
                     trend line is on the chart — this setup could never enter \
                     (draw the '{step}' line, or remove the expiry cutoff)"
                ));
            }
        } else {
            warn!(
                step = %step,
                "'{step}-expiry' cutoff line is in the past — assuming a re-arm later in time"
            );
        }
    }
    if errors.is_empty() {
        return Ok(());
    }
    Err(format!(
        "prep-expiry validation failed:\n{}\n",
        errors.join("\n")
    ))
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
        expiry_bars: args.expiry_bars,
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
        needs_confirmed: args.require_confirmation,
        close_on_news: !roles.news_pairs.is_empty(),
        sr_reversal_ranges: build_sr_ranges(roles, args.reversal_band_pct),
        needs_confirmed_close: false,
        // Populated from the chart's `<prep>-expiry` vertical lines —
        // see `prep_expiry_steps`.
        prep_expiries: prep_expiry_steps(roles),
        // H&S path: no M/W static geometry. The M/W branch (commit 9)
        // builds its spec separately, keyed on `roles.mw_path`.
        mw: None,
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

/// Parse the trade-expiry timestamp from the classified drawings.
/// Used both as the hard expiry for the trade bundle and (when known
/// pre-auto-draw) as the lookahead horizon for calendar bars so the
/// auto-draw covers the trade's full lifetime instead of just the
/// next H1+ buffer window.
fn read_trade_expiry(roles: &Roles) -> Result<DateTime<Utc>> {
    let trade_expiry_d = roles
        .trade_expiry
        .as_ref()
        .ok_or_else(|| eyre!("missing trade_expiry"))?;
    let expiry_unix = trade_expiry_d
        .points
        .first()
        .ok_or_else(|| eyre!("trade_expiry has no points"))?
        .time;
    Utc.timestamp_opt(expiry_unix, 0)
        .single()
        .ok_or_else(|| eyre!("invalid trade_expiry timestamp {expiry_unix}"))
}

/// Auto-draw vertical lines on the chart from forex-factory events
/// over the trade's lifetime. Used when the operator hasn't drawn any
/// blackout/news pairs themselves.
///
/// When `expiry_hint` is `Some`, the lookahead horizon is the trade
/// expiry (so multi-day H1+ trades pick up events past the default
/// 9h buffer). When `None` (rare: expiry drawing missing), falls back
/// to the default buffer-only window from `plan_calendar_bars`.
fn auto_draw_calendar_lines(
    mcp: &TvMcp,
    resolution: &str,
    resolved: &crate::instrument_resolution::ResolvedInstrument,
    expiry_hint: Option<DateTime<Utc>>,
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
    let events = match expiry_hint {
        Some(expiry) if expiry > now => runtime
            .block_on(cli::fetch_events_for_range(now, expiry))
            .wrap_err("fetch_events_for_range")?,
        _ => runtime
            .block_on(cli::fetch_week_events(now))
            .wrap_err("fetch_week_events")?,
    };
    let inputs = cli::PlanInputs {
        // trade_id isn't used for the line geometry — empty string is fine.
        trade_id: String::new(),
        instrument: resolved.broker_symbol.clone(),
        account: String::new(),
        broker: cli::BrokerKind::Oanda,
    };
    let plan = match expiry_hint {
        Some(expiry) if expiry > now => cli::plan_calendar_bars_within(
            &events,
            &instrument_parsed,
            timeframe.into(),
            now,
            expiry,
            &inputs,
        )
        .wrap_err("plan_calendar_bars_within")?,
        _ => cli::plan_calendar_bars(&events, &instrument_parsed, timeframe.into(), now, &inputs)
            .wrap_err("plan_calendar_bars")?,
    };
    info!(
        events_fetched = events.len(),
        events_kept = plan.rows.len(),
        horizon = ?expiry_hint.map(|e| e.to_rfc3339()),
        "calendar fetch + plan",
    );
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
            stamp_payload(&mut p, trade_id, &file, out_dir)?;
            payloads.push(p);
        }
    }

    // 2. Pause bundles.
    for bundle in pause_bundles {
        let ctx = DispatchContext {
            blackout_pair: Some((&bundle.start, &bundle.end)),
            ..Default::default()
        };
        for alert in &bundle.built.alerts {
            let file = format!("{}.yaml", alert.basename);
            if let Some(mut p) = build_alert_spec(&file, direction, roles, &ctx)? {
                stamp_payload(&mut p, trade_id, &file, &bundle.out_dir)?;
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
                stamp_payload(&mut p, trade_id, &file, &bundle.out_dir)?;
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
                stamp_payload(&mut p, trade_id, &entry.file, &bundle.bundle_dir)?;
                payloads.push(p);
            }
        }
    }
    Ok(payloads)
}

/// Stamp the orchestrator-owned fields onto a dispatched payload:
///
/// - `tv_name` → `<trade_id>-<role_slug>` so all alerts sort together
///   in TV's alert list (empty `trade_id` is a no-op).
/// - `name` → the manifest filename; the JS template echoes it back
///   in each result so the operator can attribute failures.
/// - `message` → the full text of the signed YAML on disk, which TV
///   posts to the webhook when the alert fires. An empty `message`
///   makes TV reject `create_alert` with `invalid_request`.
fn stamp_payload(
    payload: &mut AlertPayload,
    trade_id: &str,
    file: &str,
    out_dir: &Path,
) -> Result<()> {
    if !trade_id.is_empty() {
        let tv_name = payload.tv_name_mut();
        *tv_name = format!("{trade_id}-{tv_name}");
    }
    *payload.name_mut() = file.to_string();
    let signed_path = out_dir.join(file);
    let body = fs::read_to_string(&signed_path)
        .with_context(|| format!("read signed alert body {}", signed_path.display()))?;
    *payload.message_mut() = body;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use trading_view::drawings::{Point, Properties};

    fn vline(id: &str, unix: i64) -> Drawing {
        Drawing {
            id: id.to_string(),
            points: vec![Point {
                time: unix,
                price: 1.0,
            }],
            properties: Properties { text: None },
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-06-08T00:00:00Z".parse().unwrap()
    }

    #[test]
    fn prep_expiry_future_without_prep_errors() {
        // Cutoff in the future but no break-and-close trend line → error.
        let roles = Roles {
            prep_expiries: vec![(
                "break-and-close".into(),
                vline("e", now().timestamp() + 3600),
            )],
            ..Default::default()
        };
        let err = check_prep_expiries(&roles, now()).unwrap_err();
        assert!(err.contains("break-and-close-expiry"), "msg = {err}");
        assert!(err.contains("never enter"), "msg = {err}");
    }

    #[test]
    fn prep_expiry_future_with_prep_ok() {
        // Same future cutoff, but the break-and-close line is present →
        // a legitimate "pattern got too big" cutoff. No error.
        let roles = Roles {
            break_and_close: Some(vline("neck", now().timestamp() - 7200)),
            prep_expiries: vec![(
                "break-and-close".into(),
                vline("e", now().timestamp() + 3600),
            )],
            ..Default::default()
        };
        check_prep_expiries(&roles, now()).unwrap();
    }

    #[test]
    fn prep_expiry_in_past_is_warn_not_error() {
        // Cutoff already lapsed → we're re-arming later in time; warn
        // only, even with no prep drawing present.
        let roles = Roles {
            prep_expiries: vec![("retest".into(), vline("e", now().timestamp() - 3600))],
            ..Default::default()
        };
        check_prep_expiries(&roles, now()).unwrap();
    }

    #[test]
    fn prep_expiry_steps_lists_canonical_names() {
        let roles = Roles {
            prep_expiries: vec![
                ("break-and-close".into(), vline("a", 1)),
                ("retest".into(), vline("b", 2)),
            ],
            ..Default::default()
        };
        assert_eq!(prep_expiry_steps(&roles), vec!["break-and-close", "retest"]);
    }
}
