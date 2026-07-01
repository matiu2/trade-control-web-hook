//! End-to-end orchestration: read TV chart → classify drawings →
//! build trade + pause + news + calendar bundles → register the
//! signed `TradePlan` with the worker's server-side engine.
//!
//! Port of `tv_arm_hs.py::main()` (lines ~1548–2006). The library
//! calls into `trade-control-cli` directly rather than shelling out
//! to the binary (faster startup + structured errors).
//!
//! The legacy path (POST a signed alert bundle to TradingView via
//! tv-mcp, let TV fire the alerts at the webhook) has been retired:
//! the server-side cron engine is the sole producer now, so arming is
//! `--register-plan` (one signed plan the `*/15` cron evaluates).
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

use crate::args::Args;
use crate::args::PositionEntry;
use crate::geometry::{horizontal_price, pcl_exhausted_price_from_fib, tp_price_from_fib};
use crate::instrument_resolution::ResolvedInstrument;
use crate::mw_geometry;
use crate::position_trade::{core_direction, resolve_levels};
use crate::register_post::{post_intent_blocking, post_register_blocking};
use crate::roles::{Roles, SlotPref, classify};
use crate::timeframe::infer_calendar_timeframe;
use crate::trade_plan_build::{append_control_rules, build_trade_plan, resolution_to_granularity};
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
    let chart_range = mcp.get_range().wrap_err("read TV visible range")?;
    let visible = chart_range.visible_range;
    let view = (visible.from, visible.to);
    // `--start` (journaling): treat this timestamp as "live now" and find the
    // setup's drawings by searching the whole chart, ignoring the visible
    // window. Absent: the visible window scopes discovery as before.
    let start = parse_start(&args)?;
    if let Some(s) = start {
        info!(
            start = s,
            "--start set: searching the whole chart (nearest-to-start), ignoring the visible window"
        );
    }
    // The replay cursor (the "as-of" time for pruning elapsed news/blackout
    // pairs) is the right edge of the *loaded bars*, NOT the visible window:
    // when the chart is rewound the visible window still extends past the
    // last bar into empty future space, so `visible_range.to` overshoots the
    // cursor (and would prune events that are genuinely upcoming relative to
    // it). `bars_range.to` is the last actually-rendered bar = the cursor.
    // `--start` overrides it outright.
    let cursor_unix = start.unwrap_or(chart_range.bars_range.to);
    // Single-slot role selection follows the run mode (same signal as
    // `BuildStrictness` below): `--start` searches the whole chart
    // (nearest-to-start); else live arming (`--register-plan`, king when both
    // flags are set) trusts the newest drawing; an offline / replay build
    // (`--plan-out` alone) prefers the drawing belonging to the on-screen
    // window, so a rewound replay doesn't grab a recent, live-dated drawing.
    let slot_pref = if let Some(s) = start {
        SlotPref::NearestTo { start: s }
    } else if args.register_plan {
        SlotPref::LatestWins
    } else {
        SlotPref::WindowAware(view)
    };
    let mut drawings = mcp.list_drawings().wrap_err("list TV drawings")?;
    let mut roles = classify(&mcp, &drawings, view, slot_pref)?;

    let should_auto_draw =
        !args.skip_calendar_bars && roles.blackout_pairs.is_empty() && roles.news_pairs.is_empty();
    if should_auto_draw {
        // Auto-draw over the chart's visible range (`view`) so rewinding
        // an OLD trade into view still draws the news bars it overlapped.
        // The trade-expiry drawing only widens the horizon past the
        // visible right edge (live multi-day trades zoomed in tight); if
        // it's missing or unparseable we fall back to None — the visible
        // edge then bounds the window, and check_required (step 3)
        // surfaces the missing drawing as a hard error shortly anyway.
        let expiry_hint = read_trade_expiry(&roles).ok();
        // Calendar auto-draw range: `--start` bounds it to `[start, expiry]`
        // (the trade's own lifetime), so news bars are drawn relative to the
        // cursor and never past the trade end — not whatever's on screen. The
        // expiry_hint below still widens the effective end to the resolved
        // expiry, so a bare `start` start-point with no expiry can't run the
        // fetch across all of time. Else the visible range, as before.
        let calendar_range = match (start, expiry_hint) {
            (Some(s), Some(expiry)) => (s, expiry.timestamp()),
            (Some(s), None) => (s, s),
            (None, _) => view,
        };
        if let Err(e) = auto_draw_calendar_lines(
            &mcp,
            &state.resolution,
            &resolved,
            calendar_range,
            expiry_hint,
        ) {
            warn!(error = ?e, "calendar auto-draw failed; continuing with chart as-is");
        } else {
            drawings = mcp.list_drawings().wrap_err("re-list TV drawings")?;
            roles = classify(&mcp, &drawings, view, slot_pref)?;
        }
    }

    let key = read_key()?;
    let account = resolve_account(&args, broker);
    let out_dir = arm_out_dir(&raw_sym)?;
    let now = Utc::now();

    // Drop blackout/news windows that have already fully elapsed as of the
    // arm's reference time. Live (`--register-plan`) prunes against wall-clock
    // now; an offline replay (`--plan-out`) prunes against the chart's replay
    // cursor so a blackout still upcoming relative to the cursor survives a
    // historical replay. See `drop_past_control_pairs` / `pick_prune_as_of`.
    let prune_as_of = pick_prune_as_of(&args, now, cursor_unix, start);
    drop_past_control_pairs(&mut roles, prune_as_of);

    // 2c. Position-tool direct entry. When one of --market-entry /
    //     --stop-entry / --limit-entry is set, ignore the pattern
    //     machinery entirely: read the drawn long/short position tool,
    //     convert its tick-distance SL/TP to absolute prices, and POST a
    //     signed enter straight to the worker (placed on receipt). This
    //     short-circuits the whole pattern flow below.
    if let Some(mode) = args.position_entry_mode() {
        return run_position_entry(
            &args,
            mode,
            broker,
            &roles,
            &resolved,
            &instrument,
            &account,
            &key,
            now,
        );
    }

    // 3. Validate required drawings + resolve direction + build the
    //    trade spec. M/W (a path drawing is present) and H&S diverge
    //    completely here: M/W has no invalidation / TP-fib / prep
    //    drawings — direction and geometry come from the path anchors,
    //    and the worker computes entry/SL/TP from baked params. The
    //    `?`-returning resolver hard-errors on a bad setup; a clean
    //    operator-facing rejection returns Ok(1).
    let resolved_spec = if roles.mw_path.is_some() {
        // Pip size for the baked MwSpec comes from the canonical
        // instrument-lookup catalog (`asset.pip_size`), overridable via
        // --pip-size for the rare non-catalog case.
        resolve_mw_trade(
            &args,
            &roles,
            &instrument,
            &account,
            broker,
            resolved.asset.pip_size,
        )
    } else {
        // Bake the canonical instrument-lookup pip onto the H&S enter so
        // the worker scales offset_pips correctly (JPY/indices in
        // particular); --pip-size overrides for the rare non-catalog case.
        resolve_hs_trade(
            &args,
            &roles,
            &instrument,
            &account,
            broker,
            resolved.asset.pip_size,
        )
    };
    let (direction, trade_spec) = match resolved_spec {
        Ok(ds) => ds,
        Err(ResolveError::Reject(msg)) => {
            eprintln!("ERROR: {msg}");
            return Ok(1);
        }
        Err(ResolveError::Fatal(e)) => return Err(e),
    };

    info!(
        direction = direction.as_str(),
        pattern = ?trade_spec.pattern,
        trade_expiry = %trade_spec.trade_expiry.to_rfc3339(),
        sr_reversal_ranges = trade_spec.sr_reversal_ranges.len(),
        news_pairs = roles.news_pairs.len(),
        blackout_pairs = roles.blackout_pairs.len(),
        "trade spec built",
    );
    // `--plan-out` without `--register-plan` is an offline build (no worker
    // POST) — typically replaying / inspecting a historical setup, where an
    // already-elapsed trade_expiry (or in-window news) is expected. Relax the
    // time-sensitive checks to warnings so the JSON still gets written; any
    // path that actually arms the worker (`--register-plan`) stays strict.
    let strictness = if args.register_plan {
        cli::BuildStrictness::Strict
    } else {
        cli::BuildStrictness::Lenient
    };
    // Capture the expiry before `trade_spec` is consumed — it bounds the
    // supplemental calendar window in step 8 (`calendar_window`).
    let trade_expiry = trade_spec.trade_expiry;
    let built_trade =
        cli::build_trade_from_spec(trade_spec, now, strictness).wrap_err("build trade bundle")?;
    let trade_id = built_trade.trade_id.clone();
    cli::write_trade(&built_trade, &key, &out_dir).wrap_err("write trade bundle")?;
    info!(
        trade_id = %trade_id,
        out_dir = %out_dir.display(),
        alerts = built_trade.alerts.len(),
        "trade bundle written"
    );

    // 6. Pause bundles per blackout pair. Built against the prune as-of (replay
    //    cursor offline, wall-clock now live) so a blackout that survived the
    //    prune as still-upcoming-vs-cursor isn't then rejected as "stale" by
    //    `build_pause_from_spec`'s own past-window guard.
    let pause_bundles = build_pause_bundles(
        &roles,
        &trade_id,
        &instrument,
        &account,
        broker,
        &out_dir,
        &key,
        prune_as_of.at,
    )?;

    // 7. News bundles per news pair (same as-of reasoning as pause bundles).
    let news_bundles = build_news_bundles(
        &roles,
        &trade_id,
        &instrument,
        &account,
        broker,
        &out_dir,
        &key,
        prune_as_of.at,
    )?;

    // 8. Calendar bundles (skipped if auto-draw already handled it,
    //    or if --skip-calendar-bars was passed). When auto-draw ran,
    //    the operator now has blackout-/news-pairs on the chart, so
    //    we skip the cli's calendar-bars step to avoid double-arming.
    let built_calendar = if should_auto_draw || args.skip_calendar_bars {
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
            prune_as_of.at,
            view,
            trade_expiry,
        )?
    };

    // 8b. (--register-plan) Fold the whole trade — main alert conditions PLUS
    //     the pause/news/calendar control bars built above — into ONE signed
    //     TradePlan and register it with the worker's server-side engine. This
    //     is now the *only* way a trade is armed (the legacy TV-alert POST path
    //     was retired once the engine became the sole producer). A failed
    //     register is a hard error, but the signed bundle is already on disk.
    // `--plan-out` alone builds the plan and writes the JSON without touching
    // the worker; `--register-plan` additionally POSTs it. Run the block for
    // either so `--plan-out` is no longer a silent no-op on its own.
    if args.register_plan || args.plan_out.is_some() {
        // 8a. (--replace) Re-arm: delete the prior plan for this instrument before
        //     registering the fresh one, so the old plan stops ticking and the
        //     new one starts with clean engine state. No-op when --replace absent.
        //     Only meaningful when actually registering.
        if args.register_plan
            && let Some(replace_target) = args.replace.as_deref()
        {
            replace_existing_plan(replace_target, &built_trade.instrument, &key, now)?;
        }
        register_trade_plan(
            &built_trade,
            direction,
            &roles,
            &state.resolution,
            &pause_bundles,
            &news_bundles,
            &built_calendar,
            &key,
            &account,
            now,
            args.shadow,
            args.plan_out.as_deref(),
            args.register_plan,
            start,
        )?;
    }

    // The TradingView-alert creation path (build payloads → POST via tv-mcp) was
    // retired once the server-side cron engine became the sole producer. Arming a
    // trade is now: build + sign the bundle to disk (above) and register it as a
    // `TradePlan` with the worker (step 8b, gated on `--register-plan`).
    info!(trade_id = %trade_id, "signed bundle on disk; arm via --register-plan");
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

/// Outcome of trade-spec resolution. `Reject` is an operator-facing
/// "fix your chart / flags" message (printed, process exits 1); `Fatal`
/// is an internal failure that propagates as an error.
#[derive(Debug)]
enum ResolveError {
    Reject(String),
    Fatal(color_eyre::eyre::Error),
}

impl From<color_eyre::eyre::Error> for ResolveError {
    fn from(e: color_eyre::eyre::Error) -> Self {
        ResolveError::Fatal(e)
    }
}

/// H&S / IH&S path: validate the constellation of drawings, read
/// direction from the invalidation label, TP from the fib, expiry from
/// the vertical line, and build the spec. This is the pre-M/W flow,
/// unchanged in behaviour — just lifted into a resolver so `run` can
/// dispatch on pattern.
fn resolve_hs_trade(
    args: &Args,
    roles: &Roles,
    instrument: &str,
    account: &str,
    broker: Broker,
    catalog_pip: f64,
) -> std::result::Result<(Direction, cli::TradeSpec), ResolveError> {
    if let Err(msg) = check_required(roles, args) {
        return Err(ResolveError::Reject(msg));
    }
    // A future prep-expiry cutoff with no matching prep drawing would
    // arm a setup that can never enter; a past cutoff is just a re-arm.
    if let Err(msg) = check_prep_expiries(roles, Utc::now()) {
        return Err(ResolveError::Reject(msg));
    }
    let inv_label = roles.invalidation_label.clone().unwrap_or_default();
    let direction = Direction::from_invalidation_label(&inv_label)
        .ok_or_else(|| eyre!("invalid invalidation label {inv_label:?}"))?;
    let tp_fib = roles
        .tp_fib
        .as_ref()
        .ok_or_else(|| eyre!("missing tp_fib (already checked by check_required)"))?;
    let tp = tp_price_from_fib(&tp_fib.prices(), direction);
    // Continuous at-entry level vetos (Bug #12): the pcl-exhausted (`too-low`)
    // and invalidation (`too-high`) levels, baked onto the enter so the worker
    // rejects an entry already past either — independent of the cross-guard.
    let entry_level_vetos = hs_entry_level_vetos(roles, direction);
    let expiry = read_trade_expiry(roles)?;
    // --pip-size overrides the canonical catalog value when set.
    let pip_size = args.pip_size.unwrap_or(catalog_pip);
    let spec = build_trade_spec(
        args,
        instrument,
        account,
        broker,
        direction,
        expiry,
        tp,
        roles,
        pip_size,
        entry_level_vetos,
    );
    Ok((direction, spec))
}

/// M/W path: direction and geometry come from the 3-anchor path drawing
/// (`mw_path`), not from the H&S drawing constellation. Required
/// drawings are just the path + the trade-expiry line.
///
/// This is the live wrapper: it runs the cheap chart guards first (so an
/// operator chart mistake fails fast without a network round-trip), then
/// reads the broker spread live and delegates to
/// [`resolve_mw_trade_with_spread`] for the geometry gates + baking. The
/// pure inner fn is what the unit tests drive.
fn resolve_mw_trade(
    args: &Args,
    roles: &Roles,
    instrument: &str,
    account: &str,
    broker: Broker,
    catalog_pip: f64,
) -> std::result::Result<(Direction, cli::TradeSpec), ResolveError> {
    // --pip-size overrides the canonical catalog value when set.
    let pip_size = args.pip_size.unwrap_or(catalog_pip);
    check_mw_required(roles)?;
    // The arm-time broker spread is read live (OANDA /pricing or the
    // TradeNation chart endpoint) and baked into the enter intent so the
    // worker can mid→bid/ask correct entry/SL/TP at fill time. There is
    // no operator override — a failed read hard-errors rather than bake a
    // guessed spread.
    let spread_pips = read_spread_blocking(broker, instrument, pip_size)?;
    resolve_mw_trade_with_spread(
        args,
        roles,
        instrument,
        account,
        broker,
        pip_size,
        spread_pips,
    )
}

/// Cheap, offline guards every M/W arm needs before the live spread
/// read: exactly 3 path anchors and a trade-expiry line. Run first so a
/// fat-fingered chart fails without a network round-trip.
fn check_mw_required(roles: &Roles) -> std::result::Result<(), ResolveError> {
    let path = roles
        .mw_path
        .as_ref()
        .ok_or_else(|| eyre!("resolve_mw_trade called without an mw_path"))?;
    if !matches!(path.points.len(), 3 | 4) {
        return Err(ResolveError::Reject(format!(
            "M/W path must have 3 anchors [A runup-start, B first-point, C neckline] or 4 \
             [+ D right-shoulder]; found {}",
            path.points.len()
        )));
    }
    if roles.trade_expiry.is_none() {
        return Err(ResolveError::Reject(
            "missing required drawing for M/W:\n  - vertical_line labeled 'trade-expiry'\n".into(),
        ));
    }
    Ok(())
}

/// Pure M/W resolution given an already-read `spread_pips`: direction
/// from the anchors, structure + neckline-depth gates, then bakes the
/// static `MwSpec`. No I/O — the live spread read happens in the
/// [`resolve_mw_trade`] wrapper. Unit-tested directly.
#[allow(clippy::too_many_arguments)]
fn resolve_mw_trade_with_spread(
    args: &Args,
    roles: &Roles,
    instrument: &str,
    account: &str,
    broker: Broker,
    pip_size: f64,
    spread_pips: f64,
) -> std::result::Result<(Direction, cli::TradeSpec), ResolveError> {
    check_mw_required(roles)?;
    let path = roles
        .mw_path
        .as_ref()
        .ok_or_else(|| eyre!("resolve_mw_trade_with_spread called without an mw_path"))?;
    let runup_start = path.points[0].price;
    let first_point = path.points[1].price;
    let neckline = path.points[2].price;
    // Optional 4th anchor: the drawn right shoulder (arms immediately).
    let right_shoulder = path.points.get(3).map(|p| p.price);

    let direction = mw_geometry::mw_direction_from_anchors(runup_start, first_point)
        .ok_or_else(|| ResolveError::Reject(mw_flat_first_leg_msg(runup_start, first_point)))?;
    // Coarse "is this even an M/W shape" gate (runup leg > retrace leg).
    if let Err(e) = mw_geometry::check_mw_structure(runup_start, first_point, neckline) {
        return Err(ResolveError::Reject(format!("{e}\n")));
    }
    // Neckline-retracement depth gate.
    let pct = mw_geometry::neckline_retrace_pct(runup_start, first_point, neckline);
    if let Err(msg) = gate_neckline_pct(pct, args.allow_50_pct_m_trades) {
        return Err(ResolveError::Reject(msg));
    }
    // 4-point path: reject a drawing whose right shoulder is on the wrong
    // side of the neckline or breaks the 1.3 alignment of the shorter
    // shoulder. Drawing-level validity, so it fails arm here rather than
    // silently baking a bad geometry.
    if let Some(rs) = right_shoulder
        && let Err(e) = mw_geometry::validate_right_shoulder(first_point, neckline, rs)
    {
        return Err(ResolveError::Reject(format!("{e}\n")));
    }

    // The SL-vs-spread floor (hard limit) is enforced at build time in the
    // shared `cli::build_mw_pattern` chokepoint that this resolve feeds into,
    // and again at fire time in the worker against the live spread. Not
    // duplicated here — see `cli/src/trade_patterns.rs::build_mw_pattern`.

    let expiry = read_trade_expiry(roles)?;
    let pattern = match direction {
        Direction::Short => cli::TradePattern::M,
        Direction::Long => cli::TradePattern::W,
    };
    info!(
        direction = direction.as_str(),
        pattern = ?pattern,
        runup_start, first_point, neckline,
        right_shoulder = ?right_shoulder,
        retrace_pct = %format!("{:.1}%", pct * 100.0),
        spread_pips,
        pip_size,
        "M/W path resolved",
    );
    let spec = build_mw_trade_spec(
        args,
        instrument,
        account,
        broker,
        pattern,
        expiry,
        MwSpecAnchors {
            runup_start,
            first_point,
            neckline,
            right_shoulder,
            spread_pips,
            pip_size,
        },
    );
    Ok((direction, spec))
}

/// Read the live broker spread (in pips) on a short-lived runtime.
///
/// `resolve_mw_trade` is sync (it's called from the sync `run`), but the
/// broker reads are async — so we spin a throwaway tokio runtime here,
/// the same bridge `auto_draw_calendar_lines` uses for its calendar
/// fetch. Any read failure (no token, network error, market closed,
/// degenerate spread) surfaces as a `Fatal` resolve error carrying the
/// actionable message from `spread::read_spread_pips`.
fn read_spread_blocking(
    broker: Broker,
    instrument: &str,
    pip_size: f64,
) -> std::result::Result<f64, ResolveError> {
    let runtime = tokio::runtime::Runtime::new()
        .context("starting tokio runtime for live spread read")
        .map_err(ResolveError::Fatal)?;
    runtime
        .block_on(crate::spread::read_spread_pips(
            broker, instrument, pip_size,
        ))
        .map_err(ResolveError::Fatal)
}

/// The static M/W geometry baked into the signed enter intent — a
/// complete mirror of `cli::MwSpec`. `pip_size` is the canonical catalog
/// value (or the `--pip-size` override); `spread_pips` the arm-time
/// broker spread.
struct MwSpecAnchors {
    runup_start: f64,
    first_point: f64,
    neckline: f64,
    /// `D` — the optional drawn right shoulder (4-point path).
    right_shoulder: Option<f64>,
    spread_pips: f64,
    pip_size: f64,
}

/// Gate the neckline-retracement percentage. Default ceiling is
/// `< 40%`; `--allow-50-pct-m-trades` raises it to `<= 50%`; `> 50%` is
/// always rejected. A `NaN` pct (degenerate zero-runup path) is
/// rejected too.
fn gate_neckline_pct(pct: f64, allow_50: bool) -> std::result::Result<(), String> {
    if pct.is_nan() {
        return Err("M/W neckline retracement is undefined (zero-length runup leg)\n".into());
    }
    if pct > 0.50 {
        return Err(format!(
            "M/W neckline retracement {:.1}% exceeds the hard 50% ceiling — not a valid \
             reversal\n",
            pct * 100.0
        ));
    }
    if pct >= 0.40 && !allow_50 {
        return Err(format!(
            "M/W neckline retracement {:.1}% is >= 40% — pass --allow-50-pct-m-trades to arm a \
             marginal setup up to 50%\n",
            pct * 100.0
        ));
    }
    Ok(())
}

fn mw_flat_first_leg_msg(runup_start: f64, first_point: f64) -> String {
    format!(
        "M/W path has a flat first leg (A == B): runup_start={runup_start}, \
         first_point={first_point} — cannot infer direction\n"
    )
}

/// Build the M/W trade spec: no preps, single-shot, baked `MwSpec`. The
/// worker derives entry/SL/TP from the path geometry, so `tp_price` is a
/// placeholder the M/W build path ignores (it's `None` on the enter
/// intent).
fn build_mw_trade_spec(
    args: &Args,
    instrument: &str,
    account: &str,
    broker: Broker,
    pattern: cli::TradePattern,
    expiry: DateTime<Utc>,
    anchors: MwSpecAnchors,
) -> cli::TradeSpec {
    cli::TradeSpec {
        pattern,
        instrument: instrument.to_string(),
        account: account.to_string(),
        broker: broker_to_kind(broker),
        trade_expiry: expiry,
        risk_pct: args.risk_pct.unwrap_or(1.0),
        risk_amount: args.risk_amount,
        dry_run: args.broker_dry_run,
        // M/W is single-shot: a broker rejection of a placed order is
        // terminal (no re-entry).
        max_retries: 0,
        // Order expiry is governed by trade_expiry + the cancel/abort
        // vetos; the bar-count menu is an H&S feature.
        expiry_bars: None,
        skip_preps: Vec::new(),
        entry_offset_pips: None,
        sl_offset_pips: None,
        // Both offset forms None → the shared builder applies the ATR-pct
        // default (DEFAULT_BUFFER_ATR_PCT). Unused on the M/W path (worker
        // computes geometry); the H&S enter inherits the volatility-scaled buffer.
        entry_offset_atr_pct: None,
        sl_offset_atr_pct: None,
        sl_anchor: None,
        // Worker computes the real TP (hard 1R); this field is unused on
        // the M/W build path. Set to the neckline as a harmless,
        // non-zero placeholder so any accidental serialization is sane.
        tp_price: round5(anchors.neckline),
        // M/W anchors SL via the worker-computed geometry, not an
        // absolute drawn stop.
        sl_price: None,
        entry_deadline_pct: 80,
        allow_entry: args.entry_filter_script.clone(),
        // M/W entry is always a stop order at the worker-computed level;
        // --entry-market is an H&S flag and is ignored here.
        entry_mode: cli::EntryMode::Stop,
        needs_golden: !args.skip_golden,
        needs_confirmed: args.require_confirmation,
        // No close-on-reversal for M/W (TP is a hard 1R), so news/SR
        // close coverage is not wired.
        close_on_news: false,
        sr_reversal_ranges: Vec::new(),
        veto_on_reversal: false,
        needs_confirmed_close: false,
        prep_expiries: Vec::new(),
        mw: Some(cli::MwSpec {
            neckline: anchors.neckline,
            first_point: anchors.first_point,
            runup_start: anchors.runup_start,
            right_shoulder: anchors.right_shoulder,
            spread_pips: anchors.spread_pips,
            pip_size: anchors.pip_size,
        }),
        // Mirror the M/W pip onto the top-level field (the cli M/W builder
        // also does this); keeps the worker's sizing tail on the baked pip.
        pip_size: Some(anchors.pip_size),
        blackout_close: args.blackout_close.into_core(),
        // M/W has no fib / invalidation drawing — its abort/cancel/overshoot
        // vetos cover the level guards, so no continuous entry-level vetos.
        entry_level_vetos: Vec::new(),
        // M/W is out of scope for wrong-side stop recovery (it has no
        // EntrySpec — resolves via intent.mw). Keep today's behaviour.
        recover_entry: trade_control_core::intent::RecoverEntryAction::Skip,
        // strategy-v2 (dual stop + QM enter) is H&S-only.
        strategy_v2: false,
        // Break-even on at 50% by default; `--no-breakeven` opts out,
        // `--breakeven-pct` overrides. M/W honours it exactly like H&S — the
        // worker resolves the M/W geometry at fill, so the cron's snapshot has
        // a concrete entry/TP for the 50% level.
        breakeven_pct: if args.no_breakeven {
            None
        } else {
            Some(args.breakeven_pct.unwrap_or(0.5))
        },
    }
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
    pip_size: f64,
    entry_level_vetos: Vec<trade_control_core::intent::EntryLevelVeto>,
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
        // strategy-v2 needs a non-zero max_retries on both enters: it's the
        // multi_shot flag that keeps the engine plan alive after the first
        // enter fires, so the worker retry gate can cancel the sibling's
        // resting order. Floor to 1 (a `--max-retries 0` with `--strategy-v2`
        // is rejected by validate_args, so this floor is just belt-and-braces).
        max_retries: if args.strategy_v2 {
            args.max_retries.unwrap_or(5).max(1)
        } else {
            args.max_retries.unwrap_or(5)
        },
        expiry_bars: args.expiry_bars,
        skip_preps,
        entry_offset_pips: None,
        sl_offset_pips: None,
        // Both offset forms None → the shared builder applies the ATR-pct
        // default (DEFAULT_BUFFER_ATR_PCT). Unused on the M/W path (worker
        // computes geometry); the H&S enter inherits the volatility-scaled buffer.
        entry_offset_atr_pct: None,
        sl_offset_atr_pct: None,
        sl_anchor: None,
        tp_price: round5(tp),
        // H&S anchors SL to the pattern extreme, not an absolute price.
        sl_price: None,
        entry_deadline_pct: 80,
        allow_entry: args.entry_filter_script.clone(),
        entry_mode: if args.entry_market {
            cli::EntryMode::Market
        } else {
            cli::EntryMode::Stop
        },
        needs_golden: !args.skip_golden,
        needs_confirmed: args.require_confirmation,
        close_on_news: !roles.news_pairs.is_empty(),
        sr_reversal_ranges: build_sr_ranges(roles, args.reversal_band_pct),
        veto_on_reversal: args.veto_on_reversal,
        needs_confirmed_close: false,
        // Populated from the chart's `<prep>-expiry` vertical lines —
        // see `prep_expiry_steps`.
        prep_expiries: prep_expiry_steps(roles),
        // H&S path: no M/W static geometry. The M/W branch (commit 9)
        // builds its spec separately, keyed on `roles.mw_path`.
        mw: None,
        // Baked from instrument-lookup (or --pip-size) so the worker scales
        // the entry/SL offset_pips with the right pip, not its forex default.
        pip_size: Some(pip_size),
        blackout_close: args.blackout_close.into_core(),
        entry_level_vetos,
        // Wrong-side stop recovery (H&S / iH&S). Explicit `--recover-entry`
        // wins; otherwise a confirmation-required setup defaults to `limit`
        // (the confirmation lag is what strands the stop), and everything
        // else keeps today's drop (`skip`).
        recover_entry: args.recover_entry.map(|r| r.into_core()).unwrap_or(
            if args.require_confirmation {
                trade_control_core::intent::RecoverEntryAction::Limit
            } else {
                trade_control_core::intent::RecoverEntryAction::Skip
            },
        ),
        strategy_v2: args.strategy_v2,
        // Break-even on at 50% by default; `--no-breakeven` opts out,
        // `--breakeven-pct` overrides the threshold.
        breakeven_pct: if args.no_breakeven {
            None
        } else {
            Some(args.breakeven_pct.unwrap_or(0.5))
        },
    };
    if args.sl_from_recent {
        spec.sl_anchor = Some(match direction {
            Direction::Short => cli::PriceAnchor::RecentHigh,
            Direction::Long => cli::PriceAnchor::RecentLow,
        });
    }
    spec
}

/// The continuous at-entry level vetos for an H&S/IH&S setup (Bug #12).
///
/// Two levels, mirroring the `intent.vetos` name-list the enter already
/// carries:
/// - **pcl-exhausted** — `midpoint + 0.8 × (TP − midpoint)` from the fib;
///   the move is mostly done, a late entry's R:R no longer justifies opening.
///   For a short the entry is "past" when **at or below** it (`Below`); a long
///   mirrors (`Above`). Named `too-low` (short) / `too-high` (long).
/// - **invalidation** — the operator's horizontal at the right shoulder; the
///   thesis is dead once price runs back past it. For a short "past" is **at
///   or above** (`Above`); a long mirrors (`Below`). Named `too-high` (short)
///   / `too-low` (long).
///
/// A level that comes back `NaN` (drawing absent or malformed) is skipped so a
/// missing fib / invalidation can't bake a poison level. Direction picks both
/// the name and the side.
fn hs_entry_level_vetos(
    roles: &Roles,
    direction: Direction,
) -> Vec<trade_control_core::intent::EntryLevelVeto> {
    use trade_control_core::intent::{EntryLevelVeto, VetoSide};
    let mut out = Vec::new();

    // pcl-exhausted (the "ran most of the way to TP" gate).
    if let Some(fib) = roles.tp_fib.as_ref() {
        let level = pcl_exhausted_price_from_fib(&fib.prices(), direction);
        if level.is_finite() {
            let (name, past) = match direction {
                Direction::Short => ("too-low", VetoSide::Below),
                Direction::Long => ("too-high", VetoSide::Above),
            };
            out.push(EntryLevelVeto {
                name: name.into(),
                level,
                past,
            });
        }
    }

    // invalidation (the right-shoulder horizontal; thesis dead past it).
    if let Some(inv) = roles.invalidation.as_ref() {
        let level = horizontal_price(&inv.prices());
        if level.is_finite() {
            let (name, past) = match direction {
                Direction::Short => ("too-high", VetoSide::Above),
                Direction::Long => ("too-low", VetoSide::Below),
            };
            out.push(EntryLevelVeto {
                name: name.into(),
                level,
                past,
            });
        }
    }

    out
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

/// Drop blackout/news pairs whose window has already fully closed
/// (`end_time <= now`). The visible-window filter in [`classify`] only
/// removes lines that are *off-screen*; when the operator arms off a chart
/// that is showing historical bars (an old H&S whose trade-expiry is in the
/// past), the news/blackout vertical lines are genuinely on-screen yet their
/// window has elapsed in wall-clock terms. A past window has nothing left to
/// pause / close-on-news for, so arming it is meaningless — and feeding it to
/// `build_pause_from_spec` would hard-fail with "refusing to arm a stale
/// blackout". Drop it here, once, so the log line, `close_on_news`, and both
/// bundle builders all see a consistent live-only view.
fn drop_past_control_pairs(roles: &mut Roles, as_of: AsOf) {
    let as_of_unix = as_of.at.timestamp();
    let is_past = |pair: &(Drawing, Drawing)| pair.1.anchor_time_seconds() <= as_of_unix;
    for (kind, pairs) in [
        ("blackout", &mut roles.blackout_pairs),
        ("news", &mut roles.news_pairs),
    ] {
        let before = pairs.len();
        pairs.retain(|p| !is_past(p));
        let dropped = before - pairs.len();
        if dropped > 0 {
            info!(
                kind,
                dropped,
                as_of = %as_of.at.to_rfc3339(),
                source = as_of.source,
                "dropping control pair(s) whose window already closed (end_time <= as_of)"
            );
        }
    }
}

/// The "as-of" time control pairs are pruned against, plus where it came from
/// (for the drop log line). In a live `--register-plan` arm this is wall-clock
/// `now`; in an offline / replay `--plan-out` build it's the chart's replay
/// cursor (visible range right edge), so blackouts still *upcoming* relative to
/// the cursor survive a historical replay. See `BUG-tv-arm-stale-blackout-*`.
#[derive(Clone, Copy)]
struct AsOf {
    at: DateTime<Utc>,
    source: &'static str,
}

/// Parse `--start` to a unix second, or `None` if the flag is absent. A
/// malformed value is a hard error — unlike `--as-of` (which falls back to the
/// cursor), `--start` fundamentally changes discovery, so a typo must not
/// silently revert to visible-window matching.
fn parse_start(args: &Args) -> Result<Option<i64>> {
    let Some(raw) = args.start.as_deref() else {
        return Ok(None);
    };
    let ts = DateTime::parse_from_rfc3339(raw)
        .wrap_err_with(|| format!("--start is not valid RFC3339: {raw:?}"))?;
    Ok(Some(ts.with_timezone(&Utc).timestamp()))
}

/// Pick the as-of time used to prune already-elapsed control pairs.
///
/// - `--register-plan` (live arm): always wall-clock `now`. A genuinely stale
///   event must still be dropped when arming the live worker.
/// - `--start <ts>`: the start cursor (overrides even a live arm — `--start`
///   is an explicit "treat now as this" directive).
/// - `--as-of <ts>` (offline override): the explicit cursor, for headless /
///   cron replays with no readable chart range.
/// - offline `--plan-out` (replay): the chart's replay cursor (`bars_range.to`,
///   the last loaded bar — NOT the visible-window edge, which overshoots into
///   empty future space on a rewound chart), clamped to `now` so a normal live
///   `--plan-out` (cursor ≈ today) is unchanged and only a rewound replay
///   (cursor in the past) shifts the yardstick.
fn pick_prune_as_of(args: &Args, now: DateTime<Utc>, cursor_unix: i64, start: Option<i64>) -> AsOf {
    if let Some(s) = start
        && let Some(at) = DateTime::<Utc>::from_timestamp(s, 0)
    {
        return AsOf {
            at,
            source: "start-flag",
        };
    }
    if args.register_plan {
        return AsOf {
            at: now,
            source: "wallclock",
        };
    }
    if let Some(raw) = args.as_of.as_deref() {
        match DateTime::parse_from_rfc3339(raw) {
            Ok(ts) => {
                return AsOf {
                    at: ts.with_timezone(&Utc),
                    source: "as-of-flag",
                };
            }
            Err(e) => warn!(
                as_of = raw,
                error = %e,
                "--as-of is not valid RFC3339; falling back to the replay cursor"
            ),
        }
    }
    let cursor = DateTime::<Utc>::from_timestamp(cursor_unix, 0).unwrap_or(now);
    AsOf {
        at: cursor.min(now),
        source: "replay-cursor",
    }
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

/// Position-tool direct entry. Read the drawn long/short position tool,
/// convert its tick-distance SL/TP to absolute prices via the catalog
/// `tick_size`, build + sign a naked enter, and POST it straight to the
/// worker (placed on receipt). Returns the process exit code: `1` for a
/// clean operator-facing rejection (no position drawn, stop/limit not
/// supported yet), propagated `Err` for a real failure.
#[allow(clippy::too_many_arguments)]
fn run_position_entry(
    args: &Args,
    mode: PositionEntry,
    broker: Broker,
    roles: &Roles,
    resolved: &ResolvedInstrument,
    instrument: &str,
    account: &str,
    key: &[u8; KEY_LEN],
    now: DateTime<Utc>,
) -> Result<i32> {
    let Some(pos) = roles.position.as_ref() else {
        eprintln!(
            "ERROR: --{}-entry was set but no long/short position tool is drawn on the chart.",
            match mode {
                PositionEntry::Market => "market",
                PositionEntry::Stop => "stop",
                PositionEntry::Limit => "limit",
            }
        );
        return Ok(1);
    };

    // Tick-distance SL/TP → absolute prices. tick_size is the catalog
    // value (NOT pip_size — see position_trade docs).
    let levels = resolve_levels(pos, resolved.asset.tick_size)?;

    // Expiry: a drawn trade-expiry line wins; otherwise now + flag hours.
    let trade_expiry = match read_trade_expiry(roles) {
        Ok(t) => t,
        Err(_) => now + chrono::Duration::hours(i64::from(args.expiry_hours)),
    };

    let kind = match mode {
        PositionEntry::Market => cli::PositionEntryKind::Market,
        PositionEntry::Stop => cli::PositionEntryKind::Stop,
        PositionEntry::Limit => cli::PositionEntryKind::Limit,
    };
    let direction = core_direction(pos.direction);

    info!(
        instrument,
        direction = ?direction,
        mode = ?mode,
        entry = levels.entry,
        stop_loss = levels.stop_loss,
        take_profit = levels.take_profit,
        tick_size = resolved.asset.tick_size,
        trade_expiry = %trade_expiry.to_rfc3339(),
        "position-tool direct entry"
    );

    let spec = cli::PositionEnterSpec {
        instrument: instrument.to_string(),
        account: account.to_string(),
        broker: broker_to_kind(broker),
        direction,
        kind,
        entry_price: levels.entry,
        stop_loss: levels.stop_loss,
        take_profit: levels.take_profit,
        trade_expiry,
        risk_amount: args.risk_amount,
        pip_size: args.pip_size.or(Some(resolved.asset.pip_size)),
        dry_run: args.broker_dry_run,
    };

    let (trade_id, signed_body) = match cli::build_position_enter(&spec, key, now) {
        Ok(v) => v,
        // Build/validation failure (bad geometry, sign error) — clean rejection.
        Err(e) => {
            eprintln!("ERROR: {e}");
            return Ok(1);
        }
    };

    // Persist the signed body for audit (same place pattern bundles land).
    let out_dir = arm_out_dir(instrument)?;
    let body_path = out_dir.join(format!("{trade_id}-enter.yaml"));
    fs::write(&body_path, &signed_body)
        .with_context(|| format!("writing {}", body_path.display()))?;

    // The whole point of the position path: POST straight to the worker,
    // which places the order on receipt.
    let resp = post_intent_blocking(signed_body).wrap_err("POST position enter to worker")?;
    info!(trade_id = %trade_id, worker_response = %resp.trim(), "position enter POSTed");
    println!("entered: trade_id={trade_id} — {}", resp.trim());
    Ok(0)
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

/// Resolve the calendar event window `[window_start, lookahead_end]` from
/// the chart's visible range (`view = (from_unix, to_unix)`), widening the
/// right edge to the trade expiry when that sits past the visible edge.
///
/// The left edge (`window_start`) is the load-bearing fix for old trades:
/// events before it are dropped, events after are kept regardless of `now`,
/// so a trade rewound into view still draws the bars it overlapped. Returns
/// `None` when the window is empty (`lookahead_end <= window_start`).
fn calendar_window(
    view: (i64, i64),
    expiry_hint: Option<DateTime<Utc>>,
) -> Result<Option<(DateTime<Utc>, DateTime<Utc>)>> {
    let window_start = Utc
        .timestamp_opt(view.0, 0)
        .single()
        .ok_or_else(|| eyre!("invalid visible-range start {}", view.0))?;
    let visible_end = Utc
        .timestamp_opt(view.1, 0)
        .single()
        .ok_or_else(|| eyre!("invalid visible-range end {}", view.1))?;
    let lookahead_end = match expiry_hint {
        Some(expiry) => expiry.max(visible_end),
        None => visible_end,
    };
    if lookahead_end <= window_start {
        return Ok(None);
    }
    Ok(Some((window_start, lookahead_end)))
}

/// Auto-draw vertical lines on the chart from forex-factory events
/// over the window the operator is looking at. Used when the operator
/// hasn't drawn any blackout/news pairs themselves.
///
/// The window is the chart's **visible range** (`view = (from, to)`),
/// so it works for an OLD trade rewound into view — events the trade
/// actually overlapped (all in the past relative to `now`) are still
/// fetched and kept. The right edge is widened to the trade expiry when
/// that is later than the visible edge, so a live multi-day H1+ trade
/// zoomed in tight still picks up events out to expiry.
fn auto_draw_calendar_lines(
    mcp: &TvMcp,
    resolution: &str,
    resolved: &crate::instrument_resolution::ResolvedInstrument,
    view: (i64, i64),
    expiry_hint: Option<DateTime<Utc>>,
) -> Result<()> {
    let timeframe = infer_calendar_timeframe(resolution).ok_or_else(|| {
        eyre!("chart resolution {resolution:?} is below 15m; calendar bars skipped")
    })?;
    let (window_start, lookahead_end) = match calendar_window(view, expiry_hint)? {
        Some(w) => w,
        None => {
            info!("visible window is empty — nothing to auto-draw");
            return Ok(());
        }
    };
    // Synthesise the tcm Instrument straight from the catalog Asset
    // so non-FX assets (SMI, gold, indices) get correct news-currency
    // exposure without the FX-only cli::parse_instrument path.
    let instrument_parsed =
        crate::instrument_resolution::synthesize_calendar_instrument(resolved.asset);
    let runtime = tokio::runtime::Runtime::new().context("starting tokio runtime")?;
    let events = runtime
        .block_on(cli::fetch_events_for_range(window_start, lookahead_end))
        .wrap_err("fetch_events_for_range")?;
    let inputs = cli::PlanInputs {
        // trade_id isn't used for the line geometry — empty string is fine.
        trade_id: String::new(),
        instrument: resolved.broker_symbol.clone(),
        account: String::new(),
        broker: cli::BrokerKind::Oanda,
    };
    let plan = cli::plan_calendar_bars_within(
        &events,
        &instrument_parsed,
        timeframe.into(),
        window_start,
        lookahead_end,
        &inputs,
    )
    .wrap_err("plan_calendar_bars_within")?;
    info!(
        events_fetched = events.len(),
        events_kept = plan.rows.len(),
        window_start = %window_start.to_rfc3339(),
        lookahead_end = %lookahead_end.to_rfc3339(),
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
///
/// Returns the in-memory [`cli::BuiltCalendarBundle`]s (carrying the signed
/// pause/news intents + window times, so `--register-plan` can fold the same
/// control actions into the `TradePlan` without re-parsing the YAML).
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
    // The arm's reference time — wall-clock now for a live `--register-plan`
    // arm, the replay cursor for an offline `--plan-out` build. Passed through
    // to `run_calendar_bars`, whose `build_pause_from_spec` /
    // `build_news_from_spec` reject a pair whose window ended before it. Using
    // the cursor here (not wall-clock now) is the second half of the
    // stale-blackout fix: it keeps a historical replay's still-upcoming
    // calendar bars from being rejected as "stale". See `pick_prune_as_of`.
    as_of: DateTime<Utc>,
    view: (i64, i64),
    trade_expiry: DateTime<Utc>,
) -> Result<Vec<cli::BuiltCalendarBundle>> {
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
    // Window the supplemental calendar over the visible chart range
    // (widened to the trade expiry), so arming an OLD trade with manually
    // drawn pairs still picks up the calendar events it overlapped instead
    // of only this week's future events.
    let window = calendar_window(view, Some(trade_expiry))?;
    let built = match cli::run_calendar_bars(cb_args, *key, as_of, window) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = ?e, "calendar-bars failed; continuing without it");
            return Ok(Vec::new());
        }
    };
    info!(count = built.len(), "calendar bundles built");
    Ok(built)
}

/// One registered plan as seen in the `plan-list` response — only the two
/// fields `--replace` needs to resolve a target. Other fields are ignored.
#[derive(serde::Deserialize)]
struct PlanListEntry {
    trade_id: String,
    instrument: String,
}

/// Decide which trade_id `--replace` should delete.
///
/// - An explicit, non-empty `target` is used verbatim (delete exactly that).
/// - An empty `target` (bare `--replace`) auto-resolves by instrument: exactly
///   one registered plan on `instrument` → delete it; none → `Ok(None)`
///   (nothing to clear, proceed); more than one → a hard error naming the
///   candidates so the operator re-runs with an explicit id.
///
/// Pure (takes the parsed plan list), so the resolution rules are unit-tested
/// without the worker.
fn resolve_replace_target(
    target: &str,
    instrument: &str,
    plans: &[PlanListEntry],
) -> Result<Option<String>> {
    let target = target.trim();
    if !target.is_empty() {
        return Ok(Some(target.to_string()));
    }
    let matches: Vec<&str> = plans
        .iter()
        .filter(|p| p.instrument == instrument)
        .map(|p| p.trade_id.as_str())
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some((*only).to_string())),
        many => Err(eyre!(
            "--replace: {} plans registered for {instrument} ({}); \
             pass the trade_id explicitly: --replace <trade-id>",
            many.len(),
            many.join(", "),
        )),
    }
}

/// Re-arm support for `--register-plan`: resolve the prior plan for this
/// instrument (or the explicit `--replace <id>`) and delete it from the engine
/// before the fresh register. Queries `plan-list`, applies
/// [`resolve_replace_target`], then POSTs a signed `plan-delete` (which clears
/// both the `plan:` and `plan-state:` KV rows). A no-target resolution is a
/// logged no-op. Hard-errors on an ambiguous auto-resolve or a worker rejection
/// — better to stop than to leave a stale plan ticking beside the new one.
fn replace_existing_plan(
    target: &str,
    instrument: &str,
    key: &[u8; KEY_LEN],
    now: DateTime<Utc>,
) -> Result<()> {
    // Query the registered plans so an auto-resolve can count them per
    // instrument. Live plans only (`include_archived: false`) — a terminated
    // plan in the archive must not count against the per-instrument tally.
    let list_intent = cli::build_plan_list_intent(now, &register_suffix(now), false);
    let list_body = cli::wrap_signed(&list_intent, key, now).wrap_err("sign plan-list intent")?;
    let yaml = post_intent_blocking(list_body).wrap_err("query plan-list for --replace")?;
    let plans: Vec<PlanListEntry> =
        serde_yaml::from_str(&yaml).wrap_err("parse plan-list response")?;

    let Some(trade_id) = resolve_replace_target(target, instrument, &plans)? else {
        info!(instrument = %instrument, "--replace: no existing plan for this instrument; nothing to delete");
        return Ok(());
    };

    let del_intent = cli::build_plan_delete_intent(&trade_id, now, &register_suffix(now));
    let del_body = cli::wrap_signed(&del_intent, key, now).wrap_err("sign plan-delete intent")?;
    info!(trade_id = %trade_id, instrument = %instrument, "--replace: deleting prior registered plan");
    post_intent_blocking(del_body).wrap_err("delete prior plan for --replace")?;
    info!(trade_id = %trade_id, "--replace: prior plan deleted");
    Ok(())
}

/// Fold the built trade into one signed `register` `TradePlan` and (when
/// `register` is true) POST it to the worker's server-side engine.
///
/// When `register` is false (`--plan-out` without `--register-plan`) the plan is
/// still built and, if `plan_out` is set, written to disk — but no worker POST
/// happens. This is the offline "just give me the JSON for replay" path.
///
/// The plan re-expresses every alert's condition as an engine [`Trigger`] (via
/// [`build_trade_plan`], the inverse of `alert_spec`) and carries each alert's
/// embedded intent verbatim. The pause/news/calendar **control bars** built
/// upstream are folded in too — one `TimeReached` rule per bundle alert (see
/// [`append_control_rules`]) — so the registered plan opens/closes the same
/// blackout + news windows the legacy TV-alert path used to POST. It's
/// signed with the same key + whole-body HMAC as the control intents (the plan
/// rides `trade_plan` as single-line flow JSON, so it's fully signed) and
/// POSTed directly to the baked webhook.
///
/// Hard-errors on an unsupported chart resolution or a worker rejection — but
/// the signed alert bundle is already on disk by the time this runs, so the
/// trade isn't lost on a register failure.
#[allow(clippy::too_many_arguments)]
fn register_trade_plan(
    built_trade: &cli::BuiltTrade,
    direction: Direction,
    roles: &Roles,
    resolution: &str,
    pause_bundles: &[PauseBundle],
    news_bundles: &[NewsBundle],
    built_calendar: &[cli::BuiltCalendarBundle],
    key: &[u8; KEY_LEN],
    account: &str,
    now: DateTime<Utc>,
    shadow: bool,
    plan_out: Option<&Path>,
    register: bool,
    replay_start: Option<i64>,
) -> Result<()> {
    use cli::TradePattern;
    let is_mw = matches!(built_trade.spec.pattern, TradePattern::M | TradePattern::W);
    let granularity = resolution_to_granularity(resolution).ok_or_else(|| {
        eyre!(
            "chart resolution {resolution:?} has no engine granularity; \
             cannot register a server-side plan (supported: 1/5/15/60/240/D)"
        )
    })?;
    let mut plan = build_trade_plan(
        &built_trade.trade_id,
        &built_trade.instrument,
        &built_trade.alerts,
        direction,
        roles,
        granularity,
        is_mw,
        shadow,
        replay_start,
    );
    // Unwrap the tv-arm bundle wrappers to the cli `BuiltPause`/`BuiltNews` the
    // appender reads (each carries the signed intents + window times).
    let pauses: Vec<&cli::BuiltPause> = pause_bundles.iter().map(|b| &b.built).collect();
    let newses: Vec<&cli::BuiltNews> = news_bundles.iter().map(|b| &b.built).collect();
    append_control_rules(&mut plan, &pauses, &newses, built_calendar);
    let rule_count = plan.rules.len();
    // Dump the fully-built plan (control rules folded in) for offline replay,
    // before `build_register_intent` moves it into the register intent.
    if let Some(path) = plan_out {
        let json = serde_json::to_string_pretty(&plan).wrap_err("serialise trade plan")?;
        fs::write(path, json).wrap_err_with(|| format!("write plan to {}", path.display()))?;
        info!(path = %path.display(), "wrote trade plan JSON");
    }
    // Offline path: `--plan-out` without `--register-plan` stops here — the JSON
    // is on disk, but we never POST the plan to the worker.
    if !register {
        info!(
            trade_id = %built_trade.trade_id,
            "plan built (--plan-out only); not registering with worker"
        );
        return Ok(());
    }
    // Mint a fresh register intent carrying the plan, sign it, POST it.
    let suffix = register_suffix(now);
    let intent = cli::build_register_intent(plan, Some(account), now, &suffix);
    let body = cli::wrap_signed(&intent, key, now).wrap_err("sign register intent")?;
    info!(
        trade_id = %built_trade.trade_id,
        instrument = %built_trade.instrument,
        granularity = ?granularity,
        rules = rule_count,
        shadow = shadow,
        "registering server-side trade plan",
    );
    post_register_blocking(body).wrap_err("register trade plan with worker")?;
    info!(trade_id = %built_trade.trade_id, "trade plan registered");
    Ok(())
}

/// A short per-call tag for the register intent id so two arms of the same
/// trade_id in the same second don't collide on the worker's seen-id check.
/// Derived from the sub-second clock — no rand dependency.
fn register_suffix(now: DateTime<Utc>) -> String {
    format!("{:06}", now.timestamp_subsec_micros() % 1_000_000)
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
    use clap::Parser;
    use trading_view::drawings::{Point, Properties};

    fn vline(id: &str, unix: i64) -> Drawing {
        Drawing {
            id: id.to_string(),
            points: vec![Point {
                time: unix,
                price: 1.0,
            }],
            properties: Properties {
                text: None,
                ..Default::default()
            },
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-06-08T00:00:00Z".parse().unwrap()
    }

    fn wallclock(at: DateTime<Utc>) -> AsOf {
        AsOf {
            at,
            source: "wallclock",
        }
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

    // ===== past control-pair drop =======================================

    #[test]
    fn drop_past_control_pairs_removes_elapsed_windows() {
        // One live pair (ends in the future) and one elapsed pair (ends
        // before `now`) for each of blackout + news. Only the live pairs
        // survive — an elapsed window has nothing left to act on, and feeding
        // it to build_pause/news_from_spec would hard-fail as a "stale" arm.
        let t = now().timestamp();
        let live = (vline("ls", t + 1800), vline("le", t + 3600));
        let past = (vline("ps", t - 7200), vline("pe", t - 3600));
        let mut roles = Roles {
            blackout_pairs: vec![past.clone(), live.clone()],
            news_pairs: vec![past.clone(), live.clone()],
            ..Default::default()
        };

        drop_past_control_pairs(&mut roles, wallclock(now()));

        assert_eq!(roles.blackout_pairs.len(), 1);
        assert_eq!(roles.blackout_pairs[0].1.id, "le");
        assert_eq!(roles.news_pairs.len(), 1);
        assert_eq!(roles.news_pairs[0].1.id, "le");
    }

    #[test]
    fn drop_past_control_pairs_keeps_window_ending_exactly_now() {
        // Boundary: a window whose end is in the future by one second is
        // live; one ending exactly at `now` is treated as elapsed (the gate
        // is `end <= now`), mirroring build_pause_from_spec's own check.
        let t = now().timestamp();
        let mut roles = Roles {
            news_pairs: vec![
                (vline("a_s", t - 60), vline("a_e", t)), // ends exactly now → past
                (vline("b_s", t), vline("b_e", t + 1)),  // ends 1s out → live
            ],
            ..Default::default()
        };

        drop_past_control_pairs(&mut roles, wallclock(now()));

        assert_eq!(roles.news_pairs.len(), 1);
        assert_eq!(roles.news_pairs[0].1.id, "b_e");
    }

    // ===== as-of selection for control-pair pruning =====================

    /// Replay regression (the bug): a `--plan-out` build off a rewound chart
    /// must prune against the replay cursor (`bars_range.to`, the last loaded
    /// bar), so an event AHEAD of the cursor — but BEFORE wall-clock today — is
    /// kept, not dropped.
    #[test]
    fn pick_prune_as_of_offline_uses_replay_cursor() {
        let now = now(); // 2026-06-08
        let cursor = "2026-05-28T21:00:00Z".parse::<DateTime<Utc>>().unwrap();

        let as_of = pick_prune_as_of(
            &mw_args(&["--plan-out", "/tmp/x.json"]),
            now,
            cursor.timestamp(),
            None,
        );

        assert_eq!(as_of.at, cursor);
        assert_eq!(as_of.source, "replay-cursor");

        // An event 12h after the cursor (still in the past vs `now`) survives.
        let event_end = cursor.timestamp() + 12 * 3600;
        let mut roles = Roles {
            blackout_pairs: vec![(vline("s", event_end - 1800), vline("e", event_end))],
            ..Default::default()
        };
        drop_past_control_pairs(&mut roles, as_of);
        assert_eq!(
            roles.blackout_pairs.len(),
            1,
            "upcoming-vs-cursor pair kept"
        );
    }

    /// Live arm: `--register-plan` always prunes against wall-clock now, even
    /// though the chart cursor (a tightly-zoomed live view) may sit in the past.
    #[test]
    fn pick_prune_as_of_register_plan_uses_wallclock() {
        let now = now();
        let cursor = "2026-05-28T21:00:00Z".parse::<DateTime<Utc>>().unwrap();

        let as_of = pick_prune_as_of(
            &mw_args(&["--register-plan"]),
            now,
            cursor.timestamp(),
            None,
        );

        assert_eq!(as_of.at, now);
        assert_eq!(as_of.source, "wallclock");
    }

    /// A normal live `--plan-out` (cursor ≈ today) is unchanged: the cursor is
    /// clamped to `now`, so we never treat a future cursor as the yardstick.
    #[test]
    fn pick_prune_as_of_offline_clamps_future_cursor_to_now() {
        let now = now();
        let cursor_unix = now.timestamp() + 7200; // cursor 2h ahead of now

        let as_of = pick_prune_as_of(
            &mw_args(&["--plan-out", "/tmp/x.json"]),
            now,
            cursor_unix,
            None,
        );

        assert_eq!(as_of.at, now, "cursor clamped down to now");
    }

    /// `--as-of` overrides the cursor for headless replays with no chart range.
    #[test]
    fn pick_prune_as_of_explicit_flag_overrides_cursor() {
        let now = now();
        let forced = "2026-05-28T21:00:00Z".parse::<DateTime<Utc>>().unwrap();

        let as_of = pick_prune_as_of(
            &mw_args(&[
                "--plan-out",
                "/tmp/x.json",
                "--as-of",
                "2026-05-28T21:00:00Z",
            ]),
            now,
            now.timestamp(),
            None,
        );

        assert_eq!(as_of.at, forced);
        assert_eq!(as_of.source, "as-of-flag");
    }

    /// A malformed `--as-of` falls back to the replay cursor rather than failing.
    #[test]
    fn pick_prune_as_of_bad_flag_falls_back_to_cursor() {
        let now = now();
        let cursor = "2026-05-28T21:00:00Z".parse::<DateTime<Utc>>().unwrap();

        let as_of = pick_prune_as_of(
            &mw_args(&["--plan-out", "/tmp/x.json", "--as-of", "not-a-date"]),
            now,
            cursor.timestamp(),
            None,
        );

        assert_eq!(as_of.at, cursor);
        assert_eq!(as_of.source, "replay-cursor");
    }

    // ===== M / W neckline-% gate ========================================

    #[test]
    fn gate_neckline_pct_default_ceiling_is_40() {
        // < 40% passes without the flag.
        assert!(gate_neckline_pct(0.399, false).is_ok());
        // >= 40% needs the flag.
        assert!(gate_neckline_pct(0.40, false).is_err());
        assert!(gate_neckline_pct(0.499, false).is_err());
    }

    #[test]
    fn gate_neckline_pct_flag_raises_ceiling_to_50() {
        assert!(gate_neckline_pct(0.40, true).is_ok());
        assert!(gate_neckline_pct(0.499, true).is_ok());
        assert!(gate_neckline_pct(0.50, true).is_ok());
    }

    #[test]
    fn gate_neckline_pct_above_50_always_errors() {
        assert!(gate_neckline_pct(0.501, true).is_err());
        assert!(gate_neckline_pct(0.501, false).is_err());
    }

    #[test]
    fn gate_neckline_pct_nan_errors() {
        assert!(gate_neckline_pct(f64::NAN, true).is_err());
    }

    // ===== M / W trade-spec resolution ==================================

    fn path(id: &str, prices: [f64; 3]) -> Drawing {
        path_n(id, &prices)
    }

    fn path4(id: &str, prices: [f64; 4]) -> Drawing {
        path_n(id, &prices)
    }

    fn path_n(id: &str, prices: &[f64]) -> Drawing {
        Drawing {
            id: id.to_string(),
            points: prices
                .iter()
                .enumerate()
                .map(|(i, &p)| Point {
                    time: (i as i64 + 1) * 10,
                    price: p,
                })
                .collect(),
            properties: Properties {
                text: None,
                ..Default::default()
            },
        }
    }

    /// A representative arm-time spread (1 pip) the pure resolver bakes.
    /// The live read that produces this in `run` is exercised separately
    /// (the `spread` module's own tests + the demo protocol), not here.
    const SPREAD: f64 = 1.0;

    fn mw_args(extra: &[&str]) -> Args {
        let mut argv = vec!["tv-arm"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).expect("parse mw args")
    }

    fn mw_roles(p: Drawing) -> Roles {
        Roles {
            mw_path: Some(p),
            trade_expiry: Some(vline("exp", now().timestamp() + 86_400)),
            ..Default::default()
        }
    }

    /// Drive the pure resolver with an injected spread — what the tests
    /// use in place of `resolve_mw_trade` (which now reads the spread
    /// live over the network).
    fn resolve(
        args: &Args,
        roles: &Roles,
        instrument: &str,
        broker: Broker,
        catalog_pip: f64,
    ) -> std::result::Result<(Direction, cli::TradeSpec), ResolveError> {
        let pip_size = args.pip_size.unwrap_or(catalog_pip);
        resolve_mw_trade_with_spread(args, roles, instrument, "ms-tn-1", broker, pip_size, SPREAD)
    }

    #[test]
    fn resolve_mw_m_is_short_and_bakes_geometry() {
        // Worked M: A=1.1000, B=1.1200, C=1.1120 → pct 0.40 (needs flag).
        // No --pip-size, so the catalog pip (passed here) is baked.
        let roles = mw_roles(path("p", [1.1000, 1.1200, 1.1120]));
        let args = mw_args(&["--allow-50-pct-m-trades"]);
        let (dir, spec) = match resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001) {
            Ok(v) => v,
            Err(_) => panic!("expected Ok"),
        };
        assert_eq!(dir, Direction::Short);
        assert_eq!(spec.pattern, cli::TradePattern::M);
        assert_eq!(spec.max_retries, 0);
        assert!(spec.prep_expiries.is_empty());
        let mw = spec.mw.expect("mw baked");
        assert!((mw.neckline - 1.1120).abs() < 1e-9);
        assert!((mw.first_point - 1.1200).abs() < 1e-9);
        assert!((mw.runup_start - 1.1000).abs() < 1e-9);
        // The injected live spread flows through to the baked intent.
        assert!((mw.spread_pips - SPREAD).abs() < 1e-9);
        // Catalog pip flows through unchanged.
        assert!((mw.pip_size - 0.0001).abs() < 1e-12);
        // ...and is mirrored onto the top-level spec field.
        assert_eq!(spec.pip_size, Some(0.0001));
    }

    #[test]
    fn resolve_mw_4point_bakes_right_shoulder() {
        // 4-point M: A=1.1000, B=1.1200, C=1.1120, D=1.1190 (valid: inside
        // the 1.3 ceiling of the shorter shoulder, same side as B).
        let roles = mw_roles(path4("p", [1.1000, 1.1200, 1.1120, 1.1190]));
        let args = mw_args(&["--allow-50-pct-m-trades"]);
        let (dir, spec) = resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001)
            .expect("valid 4-point M resolves");
        assert_eq!(dir, Direction::Short);
        let mw = spec.mw.expect("mw baked");
        assert_eq!(mw.right_shoulder, Some(1.1190));
    }

    #[test]
    fn resolve_mw_4point_rejects_misaligned_right_shoulder() {
        // D=1.1300 breaks the 1.3 alignment (taller shoulder past the
        // ceiling of the shorter) → the drawing is rejected at arm.
        let roles = mw_roles(path4("p", [1.1000, 1.1200, 1.1120, 1.1300]));
        let args = mw_args(&["--allow-50-pct-m-trades"]);
        match resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001) {
            Err(ResolveError::Reject(msg)) => {
                assert!(msg.contains("1.3 alignment"), "msg = {msg}")
            }
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn resolve_mw_4point_rejects_wrong_side_right_shoulder() {
        // D=1.1100 sits below the neckline (wrong side for an M) → rejected.
        let roles = mw_roles(path4("p", [1.1000, 1.1200, 1.1120, 1.1100]));
        let args = mw_args(&["--allow-50-pct-m-trades"]);
        match resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001) {
            Err(ResolveError::Reject(msg)) => assert!(msg.contains("wrong side"), "msg = {msg}"),
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn build_trade_spec_bakes_catalog_pip_for_hs() {
        // The H&S spec carries the pip passed to build_trade_spec on its
        // top-level field (the worker scales offset_pips with it). A
        // JPY-scale 0.01 must survive, not collapse to the forex default.
        let args = mw_args(&[]);
        let spec = build_trade_spec(
            &args,
            "USD_JPY",
            "ms-oanda-1",
            Broker::Oanda,
            Direction::Short,
            now() + chrono::Duration::days(1),
            150.0,
            &Roles::default(),
            0.01,
            Vec::new(),
        );
        assert_eq!(spec.pattern, cli::TradePattern::Hs);
        assert!(spec.mw.is_none());
        assert_eq!(spec.pip_size, Some(0.01));
    }

    #[test]
    fn skip_golden_clears_needs_golden_on_hs_spec() {
        // BUG-replay-golden-gate-not-enforced (arm half): `--skip-golden`
        // must flip `needs_golden` to false on the emitted H&S spec (which
        // threads onto every enter intent — BCR stop, QM limit, v2 sibling).
        // Default (no flag) keeps it on.
        let default = build_trade_spec(
            &mw_args(&[]),
            "EUR_USD",
            "ms-oanda-1",
            Broker::Oanda,
            Direction::Long,
            now() + chrono::Duration::days(1),
            1.05,
            &Roles::default(),
            0.0001,
            Vec::new(),
        );
        assert!(
            default.needs_golden,
            "golden is on by default (every trade, always)"
        );

        let skipped = build_trade_spec(
            &mw_args(&["--skip-golden"]),
            "EUR_USD",
            "ms-oanda-1",
            Broker::Oanda,
            Direction::Long,
            now() + chrono::Duration::days(1),
            1.05,
            &Roles::default(),
            0.0001,
            Vec::new(),
        );
        assert!(
            !skipped.needs_golden,
            "--skip-golden must clear needs_golden on the spec"
        );
    }

    /// End-to-end arm: build the HS spec the real `--plan-out` path builds,
    /// then run it through the SAME `cli::build_trade_from_spec` +
    /// `build_trade_plan` the pipeline uses, and inspect every emitted
    /// `rules[*].intent.needs_golden` in the serialized plan JSON. This is the
    /// path the spec-only test missed — the bug (if any) shows here.
    fn emitted_plan_with(extra: &[&str]) -> trade_control_core::trade_plan::TradePlan {
        let args = mw_args(extra);
        let spec = build_trade_spec(
            &args,
            "EUR_USD",
            "ms-oanda-1",
            Broker::Oanda,
            Direction::Short,
            now() + chrono::Duration::days(1),
            1.05,
            &Roles::default(),
            0.0001,
            Vec::new(),
        );
        let built = cli::build_trade_from_spec(spec, now(), cli::BuildStrictness::Lenient)
            .expect("build trade bundle");
        build_trade_plan(
            &built.trade_id,
            &built.instrument,
            &built.alerts,
            trade_control_conventions::Direction::Short,
            &Roles::default(),
            trade_control_core::broker::Granularity::H1,
            false,
            false,
            None,
        )
    }

    #[test]
    fn skip_golden_clears_needs_golden_on_every_emitted_enter() {
        // BUG-replay-golden-gate-not-enforced (arm half), asserted against the
        // EMITTED PLAN JSON, not just the spec builder. `--skip-golden` with the
        // raw style (`--skip-break-and-close --skip-retest`) must yield
        // `needs_golden: false` on every ENTER rule (05-enter BCR stop, and
        // 09-enter-qm if strategy-v2). The 06-close-on-reversal guard keeps its
        // own `needs_golden: true` — that's the CLOSE gate, not the entry gate,
        // and `--skip-golden` does not touch it.
        let plan = emitted_plan_with(&["--skip-break-and-close", "--skip-retest", "--skip-golden"]);
        let json = serde_json::to_string_pretty(&plan).unwrap();

        // Only ENTER rules are governed by --skip-golden. The
        // 06-close-on-reversal guard legitimately keeps needs_golden: true —
        // that's the CLOSE gate, which --skip-golden does not touch. (This is
        // why a raw-style plan's `rules[4]` still shows true: it's the close
        // guard, not the stop enter.)
        let mut saw_enter = false;
        for rule in &plan.rules {
            if rule.intent.action == trade_control_core::intent::Action::Enter {
                saw_enter = true;
                assert!(
                    !rule.intent.needs_golden,
                    "emitted ENTER rule {} still carries needs_golden: true \
                     despite --skip-golden\nplan JSON:\n{json}",
                    rule.rule_id
                );
            }
        }
        assert!(saw_enter, "expected at least one ENTER rule in the plan");
    }

    #[test]
    fn default_keeps_needs_golden_on_every_emitted_enter() {
        // Mirror image: with no flag, every emitted ENTER rule carries
        // needs_golden: true (golden is on every trade, always).
        let plan = emitted_plan_with(&["--skip-break-and-close", "--skip-retest"]);
        let mut saw_enter = false;
        for rule in &plan.rules {
            if rule.intent.action == trade_control_core::intent::Action::Enter {
                saw_enter = true;
                assert!(
                    rule.intent.needs_golden,
                    "emitted ENTER rule {} should default to needs_golden: true",
                    rule.rule_id
                );
            }
        }
        assert!(saw_enter, "expected at least one ENTER rule in the plan");
    }

    #[test]
    fn skip_golden_clears_needs_golden_on_strategy_v2_siblings() {
        // strategy-v2 emits TWO enters (BCR stop + QM limit) — both must honour
        // --skip-golden in the emitted plan.
        // --strategy-v2 conflicts with the explicit --skip-* flags (it owns the
        // prep-skip internally), so pass it alone with --skip-golden.
        let plan = emitted_plan_with(&["--skip-golden", "--strategy-v2"]);
        let json = serde_json::to_string_pretty(&plan).unwrap();
        let enters: Vec<_> = plan
            .rules
            .iter()
            .filter(|r| r.intent.action == trade_control_core::intent::Action::Enter)
            .collect();
        assert!(
            enters.len() >= 2,
            "strategy-v2 should emit at least two enters, got {}\n{json}",
            enters.len()
        );
        for rule in enters {
            assert!(
                !rule.intent.needs_golden,
                "strategy-v2 ENTER rule {} still carries needs_golden: true despite --skip-golden",
                rule.rule_id
            );
        }
    }

    #[test]
    fn skip_golden_clears_needs_golden_on_mw_spec() {
        // Same guard on the M/W spec builder — `--skip-golden` clears it,
        // default keeps it on.
        let anchors = || MwSpecAnchors {
            runup_start: 1.0500,
            first_point: 1.1000,
            neckline: 1.0800,
            right_shoulder: None,
            spread_pips: 1.0,
            pip_size: 0.0001,
        };
        let default = build_mw_trade_spec(
            &mw_args(&[]),
            "EUR_USD",
            "ms-oanda-1",
            Broker::Oanda,
            cli::TradePattern::W,
            now() + chrono::Duration::days(1),
            anchors(),
        );
        assert!(default.needs_golden, "golden on by default for M/W too");

        let skipped = build_mw_trade_spec(
            &mw_args(&["--skip-golden"]),
            "EUR_USD",
            "ms-oanda-1",
            Broker::Oanda,
            cli::TradePattern::W,
            now() + chrono::Duration::days(1),
            anchors(),
        );
        assert!(
            !skipped.needs_golden,
            "--skip-golden must clear needs_golden on the M/W spec"
        );
    }

    #[test]
    fn resolve_hs_pip_size_flag_overrides_catalog() {
        // --pip-size beats the catalog value on the H&S path too (the
        // override is applied in resolve_hs_trade before build_trade_spec).
        let args = mw_args(&["--pip-size", "0.25"]);
        // Mirror resolve_hs_trade's override step, then build the spec.
        let pip_size = args.pip_size.unwrap_or(0.0001);
        let spec = build_trade_spec(
            &args,
            "EUR_USD",
            "ms-oanda-1",
            Broker::Oanda,
            Direction::Long,
            now() + chrono::Duration::days(1),
            1.05,
            &Roles::default(),
            pip_size,
            Vec::new(),
        );
        assert_eq!(spec.pip_size, Some(0.25));
    }

    #[test]
    fn hs_entry_level_vetos_short_sides_and_skips_missing() {
        // Bug #12: a short H&S with a fib (head 1.1000 → neckline 1.0900) and
        // an invalidation horizontal at 1.1050 bakes two level vetos:
        //   too-low  = pcl-exhausted, side Below  (entry past it = too far down)
        //   too-high = invalidation,  side Above  (entry above the shoulder)
        use trade_control_core::intent::VetoSide;
        let mut roles = Roles {
            // fib: head → neckline (2 prices).
            tp_fib: Some(path_n("fib", &[1.1000, 1.0900])),
            // invalidation horizontal (1 price).
            invalidation: Some(path_n("inv", &[1.1050])),
            ..Default::default()
        };
        let vetos = hs_entry_level_vetos(&roles, Direction::Short);
        let by = |n: &str| vetos.iter().find(|v| v.name == n).expect("present");
        // pcl: midpoint 1.0950, tp = 2×1.0900 − 1.1000 = 1.0800,
        //   level = 1.0950 + 0.8×(1.0800 − 1.0950) = 1.0830.
        let low = by("too-low");
        assert_eq!(low.past, VetoSide::Below);
        assert!((low.level - 1.0830).abs() < 1e-9, "{}", low.level);
        let high = by("too-high");
        assert_eq!(high.past, VetoSide::Above);
        assert!((high.level - 1.1050).abs() < 1e-9);

        // Missing fib → only the invalidation veto is baked (NaN is skipped).
        roles.tp_fib = None;
        let vetos = hs_entry_level_vetos(&roles, Direction::Short);
        assert_eq!(vetos.len(), 1);
        assert_eq!(vetos[0].name, "too-high");
    }

    #[test]
    fn hs_entry_level_vetos_long_mirrors() {
        // IH&S long: sides flip. pcl named too-high/Above, invalidation
        // too-low/Below.
        use trade_control_core::intent::VetoSide;
        let roles = Roles {
            tp_fib: Some(path_n("fib", &[1.0900, 1.1000])), // head below neckline (long)
            invalidation: Some(path_n("inv", &[1.0850])),
            ..Default::default()
        };
        let vetos = hs_entry_level_vetos(&roles, Direction::Long);
        let pcl = vetos.iter().find(|v| v.name == "too-high").expect("pcl");
        assert_eq!(pcl.past, VetoSide::Above);
        let inv = vetos.iter().find(|v| v.name == "too-low").expect("inv");
        assert_eq!(inv.past, VetoSide::Below);
        assert!((inv.level - 1.0850).abs() < 1e-9);
    }

    #[test]
    fn resolve_mw_bakes_catalog_pip_when_no_override() {
        // A JPY-like catalog pip of 0.01 is baked when --pip-size is absent.
        let roles = mw_roles(path("p", [1.1000, 1.1200, 1.1180])); // pct 0.10
        let args = mw_args(&[]);
        let (_dir, spec) =
            resolve(&args, &roles, "USD_JPY", Broker::TradeNation, 0.01).expect("ok");
        assert!((spec.mw.expect("mw").pip_size - 0.01).abs() < 1e-12);
    }

    #[test]
    fn resolve_mw_pip_size_flag_overrides_catalog() {
        // --pip-size beats the catalog value passed in.
        let roles = mw_roles(path("p", [1.1000, 1.1200, 1.1180])); // pct 0.10
        let args = mw_args(&["--pip-size", "0.25"]);
        let (_dir, spec) =
            resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001).expect("ok");
        assert!((spec.mw.expect("mw").pip_size - 0.25).abs() < 1e-12);
    }

    #[test]
    fn resolve_mw_w_is_long() {
        // Worked W: A=1.1200, B=1.1000, C=1.1080 → pct 0.40 (needs flag).
        let roles = mw_roles(path("p", [1.1200, 1.1000, 1.1080]));
        let args = mw_args(&["--allow-50-pct-m-trades"]);
        let (dir, spec) =
            resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001).expect("ok");
        assert_eq!(dir, Direction::Long);
        assert_eq!(spec.pattern, cli::TradePattern::W);
    }

    #[test]
    fn resolve_mw_rejects_40_pct_without_flag() {
        let roles = mw_roles(path("p", [1.1000, 1.1200, 1.1120])); // pct 0.40
        let args = mw_args(&[]); // no --allow-50-pct-m-trades
        match resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001) {
            Err(ResolveError::Reject(msg)) => {
                assert!(msg.contains("40%"), "msg = {msg}");
                assert!(msg.contains("--allow-50-pct-m-trades"), "msg = {msg}");
            }
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn check_mw_required_rejects_wrong_anchor_count() {
        // A 2-anchor path fails the cheap guard before any spread read.
        let roles = Roles {
            mw_path: Some(Drawing {
                id: "p".into(),
                points: vec![
                    Point {
                        time: 10,
                        price: 1.1,
                    },
                    Point {
                        time: 20,
                        price: 1.12,
                    },
                ],
                properties: Properties {
                    text: None,
                    ..Default::default()
                },
            }),
            trade_expiry: Some(vline("exp", now().timestamp() + 86_400)),
            ..Default::default()
        };
        match check_mw_required(&roles) {
            Err(ResolveError::Reject(msg)) => {
                assert!(
                    msg.contains("3 anchors") && msg.contains("found 2"),
                    "msg = {msg}"
                )
            }
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn check_mw_required_accepts_four_anchor_path() {
        // A 4-anchor path (right shoulder drawn) passes the count guard.
        let roles = Roles {
            mw_path: Some(path4("p", [1.1000, 1.1200, 1.1120, 1.1190])),
            trade_expiry: Some(vline("exp", now().timestamp() + 86_400)),
            ..Default::default()
        };
        assert!(check_mw_required(&roles).is_ok());
    }

    #[test]
    fn check_mw_required_rejects_missing_trade_expiry() {
        let roles = Roles {
            mw_path: Some(path("p", [1.1000, 1.1200, 1.1180])),
            // no trade_expiry
            ..Default::default()
        };
        match check_mw_required(&roles) {
            Err(ResolveError::Reject(msg)) => assert!(msg.contains("trade-expiry"), "msg = {msg}"),
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn resolve_mw_rejects_bad_structure() {
        // retrace deeper than runup: A=1.1120, B=1.1200, C=1.1000.
        let roles = mw_roles(path("p", [1.1120, 1.1200, 1.1000]));
        let args = mw_args(&["--allow-50-pct-m-trades"]);
        match resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001) {
            Err(ResolveError::Reject(msg)) => assert!(msg.contains("runup leg"), "msg = {msg}"),
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }

    // ===== --replace target resolution =====

    fn plan_entry(trade_id: &str, instrument: &str) -> PlanListEntry {
        PlanListEntry {
            trade_id: trade_id.into(),
            instrument: instrument.into(),
        }
    }

    #[test]
    fn replace_explicit_target_used_verbatim() {
        // An explicit id is deleted regardless of how many plans exist.
        let plans = [
            plan_entry("hs-eurusd-aaaa", "EUR_USD"),
            plan_entry("hs-eurusd-bbbb", "EUR_USD"),
        ];
        let got = resolve_replace_target("hs-eurusd-bbbb", "EUR_USD", &plans).unwrap();
        assert_eq!(got.as_deref(), Some("hs-eurusd-bbbb"));
    }

    #[test]
    fn replace_auto_resolves_single_plan_for_instrument() {
        let plans = [
            plan_entry("hs-eurusd-aaaa", "EUR_USD"),
            plan_entry("hs-gbpusd-cccc", "GBP_USD"),
        ];
        let got = resolve_replace_target("", "EUR_USD", &plans).unwrap();
        assert_eq!(got.as_deref(), Some("hs-eurusd-aaaa"));
    }

    #[test]
    fn replace_auto_no_plan_for_instrument_is_noop() {
        let plans = [plan_entry("hs-gbpusd-cccc", "GBP_USD")];
        let got = resolve_replace_target("", "EUR_USD", &plans).unwrap();
        assert!(got.is_none(), "no plan on instrument → nothing to delete");
    }

    #[test]
    fn replace_auto_multiple_plans_is_hard_error() {
        let plans = [
            plan_entry("hs-eurusd-aaaa", "EUR_USD"),
            plan_entry("mw-eurusd-bbbb", "EUR_USD"),
        ];
        let err = resolve_replace_target("", "EUR_USD", &plans).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2 plans"), "msg = {msg}");
        assert!(msg.contains("hs-eurusd-aaaa"), "names candidates: {msg}");
        assert!(msg.contains("mw-eurusd-bbbb"), "names candidates: {msg}");
        // The error text points the operator at the *new* flag name.
        assert!(msg.contains("--replace"), "error names --replace: {msg}");
    }

    #[test]
    fn replace_whitespace_target_is_treated_as_auto() {
        // clap's default_missing_value for a bare `--replace` is "" → auto.
        let plans = [plan_entry("hs-eurusd-aaaa", "EUR_USD")];
        let got = resolve_replace_target("  ", "EUR_USD", &plans).unwrap();
        assert_eq!(got.as_deref(), Some("hs-eurusd-aaaa"));
    }
}
