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

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Context, Result, eyre};
use tracing::{info, warn};
use trade_control_cli as cli;
use trade_control_conventions::{Broker, Direction, split_symbol};
use trade_control_core::sig::KEY_LEN;

use crate::args::Args;
use crate::args::PositionEntry;
use crate::broker_kind::{broker_to_kind, kind_to_broker};
use crate::broker_read::read_mid_blocking;
use crate::calendar::{calendar_scope_range, calendar_windows, read_trade_expiry};
use crate::control_bundle::{Bundle, BundleContext, NewsKind, PauseKind};
use crate::control_windows::{AsOf, ControlWindows};
use crate::hs_resolve::resolve_hs_trade;
use crate::instrument_resolution::ResolvedInstrument;
use crate::mw_resolve::resolve_mw_trade;
use crate::news_marker::{NewsMarker, news_marker_lines};
use crate::plan_geometry::PlanGeometry;
use crate::position_trade::{core_direction, resolve_levels};
use crate::register_post::{post_intent_blocking, post_register_blocking};
use crate::resolve_error::ResolveError;
use crate::roles::{Roles, SlotPref, classify};
use crate::save_matrix;
use crate::setup_inputs::SetupInputs;
use crate::trade_plan_build::{append_control_rules, build_trade_plan, resolution_to_granularity};
use trading_view::drawings::Drawing;
use trading_view::mcp::TvMcp;

/// Output root for built bundles. Matches the Python `ARM_OUT_ROOT`
/// so a side-by-side run reuses the same paths.
const ARM_OUT_ROOT: &str = "/tmp/trade-control-arm";

/// Drive the full flow. Returns process exit code; non-zero means a
/// step failed (chart classification, build-trade, etc.). All errors
/// are logged before the function returns.
///
/// Two steps, deliberately: [`read_setup_from_chart`] does **all** the
/// TradingView and calendar I/O and returns a [`SetupInputs`]; [`arm_from_inputs`]
/// builds, signs, and registers from that value alone. Nothing below the seam
/// touches a chart, which is the property a frozen-spec arm (`--spec-in`) needs
/// — and it is enforced by the types, not by a comment: `arm_from_inputs` has no
/// `TvMcp` to call.
///
/// `Roles` is returned alongside rather than folded in, because exactly one
/// consumer still needs the raw drawings (the position-entry tools) and that
/// path is inherently live-chart. See [`SetupInputs`]' module doc.
pub fn run(args: Args) -> Result<i32> {
    // Source the setup: a frozen spec, or the live chart. `Roles` only exists on
    // the chart path — the position-entry tools need raw drawings, so a frozen
    // arm refuses them rather than silently arming something else.
    let (setup, roles) = match args.spec_in.as_deref() {
        Some(path) => (read_setup_from_spec(&args, path)?, None),
        None => {
            let (setup, roles) = read_setup_from_chart(&args)?;
            if let Some(path) = args.spec_out.as_deref() {
                freeze_setup(&args, &setup, path)?;
            }
            (setup, Some(roles))
        }
    };

    // Both sources feed the same two exits. The matrix in particular has to work
    // off a FROZEN setup: "confirm once on the chart, then generate the grid
    // offline" is the whole corpus workflow, and wiring it to the chart path
    // alone made `--spec-in --save-matrix` silently arm a single cell.
    if args.save_matrix {
        return arm_the_matrix(&args, setup, roles.as_ref());
    }
    arm_from_inputs(&args, setup, roles.as_ref())
}

/// Write the chart-derived setup to a `--spec-out` file for later `--spec-in`.
fn freeze_setup(args: &Args, setup: &SetupInputs, path: &Path) -> Result<()> {
    let frozen = crate::frozen_setup::FrozenSetup::capture(
        setup.geom.clone(),
        setup.resolution.clone(),
        setup.chart_symbol.clone(),
        setup.start,
        args.spec_note.clone(),
    );
    frozen.write(path)?;
    info!(
        path = %path.display(),
        resolution = %frozen.resolution,
        chart_symbol = %frozen.chart_symbol,
        "froze setup — re-arm with --spec-in"
    );
    Ok(())
}

/// Arm every cell of the entry-sensitivity grid from ONE chart read.
///
/// The setup is cloned per cell, so all six share byte-identical geometry — the
/// only difference between the resulting fixtures is the flag under test. Six
/// separate `tv-arm` runs could not promise that: each would re-classify roles
/// against a chart that may have scrolled and re-read a calendar that may have
/// moved, so the grid would be comparing setups rather than gates.
///
/// A cell that fails is **recorded and the run continues**. A variant can
/// legitimately be rejected (a Quasimodo leg the drawing doesn't support, a
/// validation gate that objects to one entry rule), and aborting on the first
/// would throw away the cells that did work. Exit code is 0 only if every cell
/// armed, so a driver can still trust it.
fn arm_the_matrix(args: &Args, setup: SetupInputs, roles: Option<&Roles>) -> Result<i32> {
    let outcomes: Vec<save_matrix::CellOutcome> = save_matrix::GRID
        .iter()
        .map(|variant| {
            let cell_args = variant.apply(args);
            info!(cell = %variant.fixture_suffix(), "arming matrix cell");
            let result =
                arm_from_inputs(&cell_args, setup.clone(), roles).map_err(|e| format!("{e:#}"));
            if let Err(e) = &result {
                // Loud per-cell, so a failure isn't only visible in the summary
                // at the very end of a long run.
                warn!(cell = %variant.fixture_suffix(), error = %e, "matrix cell failed to arm");
            }
            save_matrix::CellOutcome {
                variant: *variant,
                result,
            }
        })
        .collect();

    println!("\n{}", save_matrix::summarise(&outcomes));
    // Non-zero when any cell is missing: a partial grid is not a grid, and a
    // driver that only checks the exit code must not read it as complete.
    Ok(if outcomes.iter().all(save_matrix::CellOutcome::armed) {
        0
    } else {
        1
    })
}

/// Rebuild [`SetupInputs`] from a frozen spec — **no TradingView**.
///
/// The frozen half (geometry, granularity, chart symbol, cursor) is read from
/// the file. Everything else is re-resolved exactly as a live arm would:
///
/// - **broker + instrument** from the frozen chart symbol, through the same
///   `instrument-lookup` catalog. Re-resolved rather than frozen so a catalog
///   correction reaches old specs.
/// - **precision** from the catalog. A live arm prefers the TradingView
///   Symbol-info tick; with no chart there is nothing to prefer, so the catalog
///   value stands — the same fallback a live arm takes when tv-mcp is
///   unreachable.
/// - **news / blackout windows** from the calendar, via the same
///   [`resolve_control_windows`] the chart path uses. Deliberately re-read: a
///   frozen calendar answers a question about a stale week.
///
/// Note the consequence — a spec-in arm is **not** bit-reproducible across days,
/// because the calendar moves. That's why the tier-2 baseline diff tags news-ON
/// rows `[calendar]` rather than treating their movement as a regression.
fn read_setup_from_spec(args: &Args, path: &Path) -> Result<SetupInputs> {
    let frozen = crate::frozen_setup::FrozenSetup::load(path)?;
    // The position tools need drawings this path doesn't have. Refuse up front,
    // with the reason — arming them off a frozen spec would silently place a
    // different trade.
    if args.position_entry_mode().is_some() {
        return Err(eyre!(
            "--market-entry / --stop-entry / --limit-entry read the drawn position \
             tool's SL/TP from the chart, so they cannot be used with --spec-in \
             (there are no drawings in a frozen setup)"
        ));
    }

    let broker = resolve_broker(args, &frozen.chart_symbol)?;
    let resolved = crate::instrument_resolution::resolve_for_broker(&frozen.chart_symbol, broker)?;
    let instrument = resolved.broker_symbol.clone();
    // No live chart, so no TV Symbol-info to prefer — the catalog precision is
    // the answer, exactly as it is when a live arm can't reach tv-mcp.
    let effective = crate::precision::EffectivePrecision {
        pip_size: resolved.precision.pip_size,
        tick_size: resolved.precision.tick_size,
        tick_from_tv: false,
    };

    // `--start` on the command line overrides the frozen cursor, so a single
    // spec can be re-armed at several cursors (which is what the entry-rule grid
    // does). Absent, the frozen cursor stands.
    let start = parse_start(args)?.or(frozen.start);
    let cursor_unix = start.ok_or_else(|| {
        eyre!(
            "frozen setup {} has no cursor and no --start was given; a spec-in arm \
             needs to know what instant counts as \"now\"",
            path.display()
        )
    })?;
    let prune_as_of = pick_prune_as_of(args, Utc::now(), cursor_unix, start);
    let control = resolve_control_windows(
        args,
        &frozen.geom,
        &resolved,
        &frozen.resolution,
        cursor_unix,
        prune_as_of,
    );

    info!(
        path = %path.display(),
        chart_symbol = %frozen.chart_symbol,
        resolution = %frozen.resolution,
        instrument = %instrument,
        broker = broker.as_str(),
        captured_by = frozen.tv_arm_version.as_deref().unwrap_or("unknown"),
        "armed from frozen setup (no TradingView)"
    );

    let raw_symbol = split_symbol(&frozen.chart_symbol).1.to_string();
    Ok(SetupInputs {
        geom: frozen.geom,
        control,
        instrument,
        resolved,
        broker,
        effective,
        resolution: frozen.resolution,
        chart_symbol: frozen.chart_symbol,
        raw_symbol,
        start,
        prune_as_of,
    })
}

/// Read TradingView + the economic calendar into a [`SetupInputs`].
///
/// **Every** network / tv-mcp call in the arm flow lives in here. It also
/// performs the cosmetic news-marker draw, which is chart-only by definition.
fn read_setup_from_chart(args: &Args) -> Result<(SetupInputs, Roles)> {
    // 1. Read chart state + decide broker / instrument.
    let mcp = TvMcp::new(
        args.tv_mcp_root
            .clone()
            .unwrap_or_else(|| PathBuf::from(trading_view::mcp::DEFAULT_TV_MCP_ROOT)),
    );
    let state = mcp.get_state().wrap_err("read TV chart state")?;
    let (_exchange, raw_sym) = split_symbol(&state.symbol);
    let raw_sym = raw_sym.to_string();
    let broker = resolve_broker(args, &state.symbol)?;
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

    // Effective pip/tick: prefer the LIVE TradingView Symbol-info for the
    // instrument we're arming (the same tick the chart shows), falling back
    // to the instrument-lookup catalog. A tick mismatch is logged so a stale
    // catalog surfaces at arm time without blocking the trade. The
    // `--pip-size` / `--tick-size` flags still override both, downstream in
    // the trade builders.
    let effective = match mcp.get_symbol_info() {
        Ok(tv_info) => crate::precision::resolve_effective_precision(resolved.precision, &tv_info),
        Err(e) => {
            // Reading symbol-info shouldn't ever block arming — fall back to
            // the per-broker catalog precision and note why.
            warn!(error = %e, "could not read live TV symbol-info; using catalog precision");
            crate::precision::EffectivePrecision {
                pip_size: resolved.precision.pip_size,
                tick_size: resolved.precision.tick_size,
                tick_from_tv: false,
            }
        }
    };
    info!(
        pip_size = effective.pip_size,
        tick_size = effective.tick_size,
        tick_from_tv = effective.tick_from_tv,
        "resolved effective precision"
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
    // Fetch every drawing once, up front: both the note-derived `--start`
    // fallback below and role classification consume this same list.
    let drawings = mcp.list_drawings().wrap_err("list TV drawings")?;
    // `--start` (journaling): treat this timestamp as "live now" and find the
    // setup's drawings by searching the whole chart, ignoring the visible
    // window. Absent: the visible window scopes discovery as before.
    //
    // `--replay` with no explicit `--start`: fall back to a chart Note saying
    // `start` — its first anchor's time becomes the journaling cursor. Lets an
    // operator mark live-now with a note instead of typing an RFC3339 stamp.
    let start = match (parse_start(args)?, args.replay()) {
        (Some(s), _) => Some(s),
        (None, true) => crate::start_note::resolve_start_from_note(&mcp, &drawings)
            .wrap_err("resolve --replay start from chart Note")?
            .inspect(|s| {
                info!(
                    start = s,
                    "--replay with no --start: using chart Note `start` anchor as the cursor"
                );
            }),
        (None, false) => None,
    };
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
    // (nearest-to-start); else live arming (the `register` subcommand) trusts
    // the newest drawing; an offline / replay build (the `plan-out` / `replay`
    // subcommands, or no subcommand) prefers the drawing belonging to the
    // on-screen window, so a rewound replay doesn't grab a recent, live-dated
    // drawing.
    let slot_pref = if let Some(s) = start {
        SlotPref::NearestTo { start: s }
    } else if args.register_plan() {
        SlotPref::LatestWins
    } else {
        SlotPref::WindowAware(view)
    };
    // Immutable from here on: `Roles` is what the operator drew, resolved once.
    // The calendar-derived control windows that used to be mutated onto it now
    // live in their own type — see [`crate::control_windows`].
    let roles = classify(&mcp, &drawings, view, slot_pref)?;

    // Extract the chart geometry to plain data ONCE, here — immediately after role
    // resolution and BEFORE any validation. Two reasons this belongs at the top:
    //   * the wrong-drawing risk is resolved exactly once, so the same bytes that
    //     get validated are the bytes that get planned;
    //   * everything downstream (validation, TP/direction, entry-level vetos, the
    //     plan build) reads `PlanGeometry`, which is what lets a frozen spec drive
    //     an identical arm with no TradingView.
    // `roles` is still needed below for the things that AREN'T geometry: the
    // calendar windows it carries, the drawn S/R levels, and the position tool.
    let geom = PlanGeometry::from_roles(&roles);

    // The as-of instant elapsed control windows are pruned against. Live
    // (`--register-plan`) prunes against wall-clock now; an offline replay
    // (`--plan-out`) prunes against the chart's replay cursor so a blackout
    // still upcoming relative to the cursor survives a historical replay.
    // See `pick_prune_as_of`.
    let prune_as_of = pick_prune_as_of(args, Utc::now(), cursor_unix, start);

    // Resolve blackout/news windows straight from the economic calendar at real
    // event-minute precision. No chart lines are drawn and nothing is read back
    // — the old draw + re-classify round-trip (which snapped every window to a
    // bar boundary and could split a window straddling `--start` into an
    // orphaned half) is gone. `--skip-calendar-bars` opts out entirely.
    //
    // `ControlWindows::new` prunes already-elapsed windows at construction, so
    // there is no un-pruned set for anything downstream to observe.
    let control = resolve_control_windows(
        args,
        &geom,
        &resolved,
        &state.resolution,
        cursor_unix,
        prune_as_of,
    );

    // Cosmetic chart annotation (default on): draw a vertical line for exactly
    // the news events tv-arm reacts to — the armed set, post-prune, so drawn ==
    // armed. Never touches the plan; a draw failure warns and continues.
    // `--skip-calendar-bars` opts out of the whole calendar step above, leaving
    // the marker set empty, so this then draws nothing (that flag skips both the
    // news windows and their markers).
    draw_news_markers(&mcp, control.markers(), &state.resolution);

    Ok((
        SetupInputs {
            geom,
            control,
            instrument,
            resolved,
            broker,
            effective,
            resolution: state.resolution,
            chart_symbol: state.symbol,
            raw_symbol: raw_sym,
            start,
            prune_as_of,
        },
        roles,
    ))
}

/// Build, sign, and register a trade from chart-independent inputs.
///
/// Takes **no `TvMcp`** — that is the whole point of the split. Everything below
/// this line works identically whether `setup` was read from a live chart or
/// loaded from a frozen spec.
///
/// `roles` is `Some` only on the live-chart path, and is used for exactly one
/// thing: the position-entry tools (`--market-entry` / `--stop-entry` /
/// `--limit-entry`), whose SL/TP are TradingView **drawing properties** with no
/// frozen equivalent. A frozen arm passes `None`, and asking for a position
/// entry there is a clean rejection rather than a silent wrong trade.
fn arm_from_inputs(args: &Args, setup: SetupInputs, roles: Option<&Roles>) -> Result<i32> {
    let SetupInputs {
        geom,
        control,
        instrument,
        resolved,
        broker,
        effective,
        resolution,
        chart_symbol,
        raw_symbol,
        start,
        prune_as_of,
    } = setup;

    let key = read_key()?;
    let account = resolve_account(args, broker);
    let out_dir = arm_out_dir(&raw_symbol)?;
    let now = Utc::now();

    // 2c. Position-tool direct entry. When one of --market-entry /
    //     --stop-entry / --limit-entry is set, ignore the pattern
    //     machinery entirely: read the drawn long/short position tool,
    //     convert its tick-distance SL/TP to absolute prices, and POST a
    //     signed enter straight to the worker (placed on receipt). This
    //     short-circuits the whole pattern flow below.
    if let Some(mode) = args.position_entry_mode() {
        // The position tools read raw TradingView drawings (their SL/TP are
        // drawing properties), so they exist only on the live-chart path. A
        // frozen-spec arm has no `Roles` and must say so plainly rather than
        // silently arming some other trade.
        let roles = roles.ok_or_else(|| {
            eyre!(
                "--market-entry / --stop-entry / --limit-entry read the drawn position \
                 tool from the chart, so they need a live TradingView session; they \
                 cannot be used with a frozen setup"
            )
        })?;
        return run_position_entry(
            args,
            mode,
            broker,
            roles,
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
    let resolved_spec = if geom.mw_path.is_some() {
        // Pip/tick for the baked MwSpec come from `effective` — live
        // TradingView precision when available, else the instrument-lookup
        // catalog. --pip-size / --tick-size override downstream.
        resolve_mw_trade(
            args,
            &geom,
            &instrument,
            &account,
            broker,
            effective.pip_size,
            effective.tick_size,
        )
    } else {
        // Bake the effective pip AND tick onto the H&S enter: pip scales
        // offset_pips (JPY/indices), tick snaps every order price onto the
        // broker's grid so it isn't rejected as over-precise. `effective`
        // prefers live TV; --pip-size / --tick-size override downstream.
        resolve_hs_trade(
            args,
            &geom,
            control.has_news(),
            &instrument,
            &account,
            broker,
            effective.pip_size,
            effective.tick_size,
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
        news_windows = control.news().len(),
        blackout_windows = control.blackout().len(),
        "trade spec built",
    );
    // `--plan-out` without `--register-plan` is an offline build (no worker
    // POST) — typically replaying / inspecting a historical setup, where an
    // already-elapsed trade_expiry (or in-window news) is expected. Relax the
    // time-sensitive checks to warnings so the JSON still gets written; any
    // path that actually arms the worker (`--register-plan`) stays strict.
    let strictness = if args.register_plan() {
        cli::BuildStrictness::Strict
    } else {
        cli::BuildStrictness::Lenient
    };
    let built_trade =
        cli::build_trade_from_spec(trade_spec, now, strictness).wrap_err("build trade bundle")?;
    let trade_id = built_trade.trade_id.clone();
    cli::write_trade(&built_trade, &key, &out_dir).wrap_err("write trade bundle")?;

    // The `plan-out` subcommand names the JSON destination; the `replay`
    // subcommand synthesises a temp path so the register block below writes the
    // plan there and the replay can read it back. (`register`, `plan-out`, and
    // `replay` are mutually-exclusive subcommands, so at most one arm applies.)
    // For a bare invocation (no subcommand) this stays `None` and only the
    // signed bundle is written to disk.
    let effective_plan_out: Option<PathBuf> = match (args.plan_out(), args.replay()) {
        (Some(p), _) => Some(p.to_path_buf()),
        (None, true) => Some(crate::replay::plan_path(None, &trade_id)),
        (None, false) => None,
    };
    info!(
        trade_id = %trade_id,
        out_dir = %out_dir.display(),
        alerts = built_trade.alerts.len(),
        "trade bundle written"
    );

    // 6/7. One pause bundle per blackout window, one news bundle per news
    //      window. Built against the prune as-of (replay cursor offline,
    //      wall-clock now live) so a window that survived the prune as
    //      still-upcoming-vs-cursor isn't then rejected as "stale" by
    //      `build_pause_from_spec`'s own past-window guard.
    let bundle_ctx = BundleContext {
        trade_id: &trade_id,
        instrument: &instrument,
        account: &account,
        broker,
        out_dir: &out_dir,
        key: &key,
        now: prune_as_of.at,
    };
    let pause_bundles = bundle_ctx.build_all::<PauseKind>(control.blackout())?;
    let news_bundles = bundle_ctx.build_all::<NewsKind>(control.news())?;

    // 8. Calendar control bars now come from the calendar directly, resolved in
    //    step 2 into the `ControlWindows` above and built into `pause_bundles` /
    //    `news_bundles`. The old drawn-line-era supplemental `built_calendar`
    //    path was retired in PR1b.

    // 8b. (`register`) Fold the whole trade — main alert conditions PLUS the
    //     pause/news/calendar control bars built above — into ONE signed
    //     TradePlan and register it with the worker's server-side engine. This
    //     is now the *only* way a trade is armed (the legacy TV-alert POST path
    //     was retired once the engine became the sole producer). A failed
    //     register is a hard error, but the signed bundle is already on disk.
    // The `plan-out` / `replay` subcommands build the plan and write the JSON
    // without touching the worker; only `register` additionally POSTs it. Run
    // the block whenever we have a JSON destination (plan-out/replay) or are
    // arming, so `plan-out` is no longer a silent no-op on its own.
    if args.register_plan() || effective_plan_out.is_some() {
        // 8a. (`register --replace`) Re-arm: delete the prior plan for this
        //     instrument before registering the fresh one, so the old plan stops
        //     ticking and the new one starts with clean engine state. No-op when
        //     --replace absent. Only meaningful when actually registering.
        if args.register_plan()
            && let Some(replace_target) = args.replace()
        {
            replace_existing_plan(replace_target, &built_trade.instrument, &key, now)?;
        }
        // Arm-time news-sentiment snapshot: computed as of the *effective* arm
        // time (`--start` cursor when journaling, else `now`), printed for the
        // operator, and baked onto the plan for after-the-fact journalling only.
        // Fail-soft — a fetch failure yields `None` and never blocks arming.
        let armed_at = effective_arm_time(start, now);
        // `--cross-buffer-pct` is deprecated in favour of the volatility-relative
        // `--cross-buffer-atr`. If the operator still passes it, honour it (it's
        // summed on top of the ATR term) but warn — a fixed % of price is
        // volatility-blind and easy to mis-size across instruments.
        if let Some(pct) = args.cross_buffer_pct {
            tracing::warn!(
                cross_buffer_pct = pct,
                "--cross-buffer-pct is DEPRECATED (a fixed % of price is \
                 volatility-blind); prefer --cross-buffer-atr. The percent term is \
                 summed on top of the ATR term for this arm."
            );
        }
        let armed_sentiment = crate::sentiment::arm_time_sentiment(
            &resolved.asset.id,
            &resolved.asset.news_currencies,
            armed_at,
        );
        register_trade_plan(
            &built_trade,
            direction,
            &geom,
            &resolution,
            &pause_bundles,
            &news_bundles,
            &key,
            &account,
            now,
            args.shadow(),
            effective_plan_out.as_deref(),
            args.register_plan(),
            start,
            args.retest_atr_step
                .unwrap_or(trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP),
            args.cross_buffer_pct
                .unwrap_or(trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT),
            args.cross_buffer_atr
                .unwrap_or(trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR),
            args.bcr_require_golden,
            armed_sentiment,
        )?;
    }

    // The TradingView-alert creation path (build payloads → POST via tv-mcp) was
    // retired once the server-side cron engine became the sole producer. Arming a
    // trade is now: build + sign the bundle to disk (above) and register it as a
    // `TradePlan` with the worker (step 8b, gated on the `register` subcommand).
    info!(trade_id = %trade_id, "signed bundle on disk; arm via the `register` subcommand");

    // 9. (`replay`) Chain into `replay-candles` on the plan we just wrote. The
    //    register block above wrote the JSON to `effective_plan_out` (a temp
    //    path for the `replay` subcommand). The replay is a post-build
    //    convenience: a failure here surfaces as an error.
    if args.replay() {
        // Forward what this arm knew about itself, so a chained `--save` records
        // WHICH variant the fixture froze. `state.symbol` is the broker-qualified
        // chart symbol (`TRADENATION:EURUSD`) — recorded qualified on purpose: a
        // bare symbol silently resolves to the OANDA feed, so an unqualified
        // capture can be off the wrong price data and look perfectly plausible.
        let arm = crate::replay::ArmContext {
            skip_bcr: args.skip_bcr,
            strategy_v2: args.strategy_v2,
            skip_calendar_bars: args.skip_calendar_bars,
            skip_golden: args.skip_golden,
            start: args.start.as_deref(),
            chart_symbol: Some(&chart_symbol),
        };
        crate::replay::run_replay(
            effective_plan_out.as_deref(),
            &trade_id,
            broker,
            args.replay_args(),
            arm,
        )
        .wrap_err("replay after arm (--replay)")?;
    }

    Ok(0)
}

/// Resolve blackout/news windows from the economic calendar.
///
/// Shared by the live-chart and frozen-spec paths, so a `--spec-in` arm gets the
/// **same** window resolution a live arm does. That matters more than it looks:
/// news is deliberately RE-READ rather than frozen (a frozen calendar answers a
/// question about a stale week), so this is the one piece of "chart-half" work a
/// frozen arm still has to do — and doing it differently would be a silent
/// replay↔live divergence of exactly the kind the fixture corpus exists to find.
///
/// Windows come straight from the calendar at real event-minute precision. No
/// chart lines are drawn and nothing is read back — the old draw + re-classify
/// round-trip (which snapped every window to a bar boundary and could split a
/// window straddling `--start` into an orphaned half) is gone.
/// `--skip-calendar-bars` opts out entirely.
///
/// `ControlWindows::new` prunes already-elapsed windows at construction, so
/// there is no un-pruned set for anything downstream to observe. A calendar
/// failure warns and yields no windows rather than aborting the arm.
fn resolve_control_windows(
    args: &Args,
    geom: &PlanGeometry,
    resolved: &ResolvedInstrument,
    resolution: &str,
    cursor_unix: i64,
    prune_as_of: AsOf,
) -> ControlWindows {
    if args.skip_calendar_bars {
        return ControlWindows::empty();
    }
    // Scope the news filter to the trade's own lifetime `[cursor, expiry]`, NOT
    // the chart's visible area:
    //   - left edge = the cursor (`--start` when given, else the last loaded bar
    //     `bars_range.to`) — so only news at or after "live now" matters,
    //     independent of how far left the chart is scrolled;
    //   - right edge = the trade-expiry vertical, so only news the open trade
    //     could still run into is considered.
    // A missing/unparseable expiry collapses the range to empty (no windows)
    // rather than fetching across all of time; check_required surfaces the
    // absent expiry drawing as a hard error shortly anyway.
    let expiry_hint = read_trade_expiry(geom).ok();
    let calendar_range = calendar_scope_range(cursor_unix, expiry_hint);
    match calendar_windows(
        resolution,
        resolved,
        calendar_range,
        args.news_before_hours,
        args.news_after_hours,
    ) {
        Ok((blackout, news, markers)) => ControlWindows::new(blackout, news, markers, prune_as_of),
        Err(e) => {
            warn!(error = ?e, "calendar window resolution failed; continuing with no news/blackout windows");
            ControlWindows::empty()
        }
    }
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
    // A just-recovered asset isn't in the native instrument catalog yet
    // (it was only added to the legacy overlay above), so its precision
    // falls back to the legacy single-tick `Asset` value. Live TradingView
    // still overrides this downstream, so a divergent index recovered this
    // way is still armed correctly.
    let precision = crate::precision::CatalogPrecision::from_asset(leaked);
    Ok(crate::instrument_resolution::ResolvedInstrument {
        asset: leaked,
        broker_symbol,
        precision,
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

/// H&S / IH&S path: validate the constellation of drawings, read
/// direction from the **fib** (its head↔neckline, where the head is the
/// `0`-reading resolved via TradingView's `reverse` flag — *not* raw point
/// order), TP from the fib, expiry from the vertical line, and build the
/// spec. The `too-low`/`too-high` invalidation horizontal is validated to
/// sit **inside the fib's range** (a line outside it is a stale leftover
/// from a different trade) but no longer determines direction — that was
/// the source of a wrong-direction bug when a stale invalidation from
/// another setup got picked. Reading direction off raw point order was a
/// *second* wrong-direction bug (AUD/CAD 2026-07: head at `points[1]`).
/// Takes `geom` and **no `Roles`** — no chart drawing is read anywhere below
/// this line, which is the property `--spec-in` needs. The news fact arrives
/// separately as `close_on_news`, because news windows are calendar-derived
/// rather than drawn (see [`crate::control_windows`]): a spec-driven arm supplies
/// frozen geometry and re-reads the calendar, so those two must stay apart.
///
/// It used to take **both**, and the doc here justified that: `roles` was
/// forwarded to [`build_trade_spec`] "for the one thing that isn't geometry — the
/// drawn S/R levels (and the prep-expiry steps derived from them)". That was the
/// bug, not an exception. Drawn S/R levels *are* geometry; they gate whether
/// `07-close-on-sr-reversal` is armed at all, so a spec-in re-arm without them
/// would have silently changed the exit. They now live in
/// [`PlanGeometry::sr_levels`] (and the prep-expiry steps come from
/// `prep_expiry_epochs`, which was already there), so the `Roles` parameter is
/// gone. Caught by a clean-slate review, 2026-07-27 — the same class as
/// `MwPath.runup_start`: drawn geometry no *trigger* reads, so it looked
/// droppable.
///
/// Eight parameters still trips clippy; splitting them into a struct would just
/// relocate the same fields.
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
/// the bytes — needed when a downstream builder wants the key-file path
/// rather than the loaded key material.
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
///
/// `close_on_news` is passed in rather than derived here: the news windows are
/// calendar-derived, not drawn, so they live in `ControlWindows` rather than
/// `Roles` — see [`crate::control_windows`]. All this needs is the one fact
/// "does this trade have a news window", so that's what it takes.
/// Draw one cosmetic vertical line per news event tv-arm reacts to, grouped so
/// events sharing a chart bar collapse to a single line. Purely a chart
/// annotation for debugging / replay — it never affects the signed plan.
///
/// `markers` is the *armed* set (post-prune, via
/// [`ControlWindows::markers`][crate::control_windows::ControlWindows::markers]),
/// so drawn == armed.
///
/// Failure is non-fatal: a tv-mcp draw error (or an empty marker set) logs a warning
/// and returns. Unlike tv-news — which hard-errors on a half-drawn chart — a flaky
/// tv-mcp must never block a live `--register-plan`, so every line is attempted and
/// per-line failures are counted, not propagated.
fn draw_news_markers(mcp: &TvMcp, markers: &[NewsMarker], resolution: &str) {
    if markers.is_empty() {
        info!("news markers: no armed news events to draw");
        return;
    }
    let bar_secs = news_marker_lines_bar_secs(resolution);
    let lines = news_marker_lines(markers, bar_secs);
    let mut drawn = 0usize;
    let mut failed = 0usize;
    for line in &lines {
        // Price is ignored for vertical lines but the CLI requires a value.
        match mcp.draw_vertical_line(line.anchor_epoch, 1.0, &line.label) {
            Ok(s) if s.success => drawn += 1,
            Ok(s) => {
                failed += 1;
                warn!(
                    label = %line.label,
                    error = s.error.as_deref().unwrap_or("(no message)"),
                    "news markers: tv-mcp reported a failed draw; continuing",
                );
            }
            Err(e) => {
                failed += 1;
                warn!(label = %line.label, error = %e, "news markers: draw call errored; continuing");
            }
        }
    }
    info!(
        events = markers.len(),
        lines = lines.len(),
        drawn,
        failed,
        "news markers: drew news markers for the armed event set",
    );
}

/// Bar width (seconds) for grouping same-bar markers, from the chart resolution,
/// falling back to the 1h default on an unparseable resolution.
fn news_marker_lines_bar_secs(resolution: &str) -> i64 {
    crate::news_marker::resolution_to_secs(resolution)
        .unwrap_or(crate::news_marker::DEFAULT_BAR_SECS)
}

/// Parse `--start` to a unix second, or `None` if the flag is absent. A
/// malformed value is a hard error — unlike `--as-of` (which falls back to the
/// cursor), `--start` fundamentally changes discovery, so a typo must not
/// silently revert to visible-window matching.
fn parse_start(args: &Args) -> Result<Option<i64>> {
    let Some(raw) = args.start.as_deref() else {
        return Ok(None);
    };
    // Shared with `replay-candles --start` — see `cli::start_time`. Using
    // `DateTime::parse_from_rfc3339` directly here was a real trap: it demands
    // SECONDS and rejects a bare local time, so `--start 2026-06-19T17:00+10:00`
    // failed on tv-arm while working on replay-candles, even though
    // `tv-arm ... replay` forwards the very same string to it.
    let ts = cli::start_time::parse_start_end(raw)
        .wrap_err_with(|| format!("--start is not a valid datetime: {raw:?}"))?;
    Ok(Some(ts.timestamp()))
}

/// The instant a plan should be recorded as armed at.
///
/// When `--start` (the journaling replay cursor, `replay_start` as a Unix
/// second) is given, the plan is recorded *as if* it were armed at that
/// moment — so a replayed arming reads back the historical time, and the
/// arm-time news sentiment is computed as of that point. Otherwise it's the
/// real wall-clock `now`. A `replay_start` that can't be represented as a
/// `DateTime` (out-of-range) falls back to `now` rather than failing arming.
fn effective_arm_time(replay_start: Option<i64>, now: DateTime<Utc>) -> DateTime<Utc> {
    replay_start
        .and_then(|s| DateTime::<Utc>::from_timestamp(s, 0))
        .unwrap_or(now)
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
    if args.register_plan() {
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

    // Tick-distance SL/TP → absolute prices. tick_size is the per-broker
    // catalog value (NOT pip_size — see position_trade docs).
    let levels = resolve_levels(pos, resolved.precision.tick_size)?;

    // Expiry: a drawn trade-expiry line wins; otherwise now + flag hours.
    let trade_expiry = match read_trade_expiry(&PlanGeometry::from_roles(roles)) {
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
        tick_size = resolved.precision.tick_size,
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
        pip_size: args.pip_size.or(Some(resolved.precision.pip_size)),
        tick_size: args.tick_size.or(Some(resolved.precision.tick_size)),
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
/// Takes `geom`, **not `Roles`** — the geometry is extracted exactly once, in
/// [`run`], and passed down. This function used to take `&Roles` and re-derive
/// `PlanGeometry::from_roles` itself, which meant the extraction ran *twice* per
/// arm off two different borrows of the same drawings. Harmless in practice
/// (`from_roles` is pure), but it re-opened the seam `PlanGeometry` exists to
/// close: as long as a `&Roles` reaches this far, a future edit can read a
/// drawing here that no frozen spec could supply. Same reasoning as
/// `resolve_hs_trade` losing its `roles` parameter — close it by type.
#[allow(clippy::too_many_arguments)]
fn register_trade_plan(
    built_trade: &cli::BuiltTrade,
    direction: Direction,
    geom: &PlanGeometry,
    resolution: &str,
    pause_bundles: &[Bundle<PauseKind>],
    news_bundles: &[Bundle<NewsKind>],
    key: &[u8; KEY_LEN],
    account: &str,
    now: DateTime<Utc>,
    shadow: bool,
    plan_out: Option<&Path>,
    register: bool,
    replay_start: Option<i64>,
    retest_atr_step: f64,
    cross_buffer_pct: f64,
    cross_buffer_atr: f64,
    bcr_require_golden: bool,
    armed_sentiment: Option<trade_control_core::plan_sentiment::PlanSentiment>,
) -> Result<()> {
    use cli::TradePattern;
    let is_mw = matches!(built_trade.spec.pattern, TradePattern::M | TradePattern::W);
    let granularity = resolution_to_granularity(resolution).ok_or_else(|| {
        eyre!(
            "chart resolution {resolution:?} has no engine granularity; \
             cannot register a server-side plan (supported: 1/5/15/60/240/D)"
        )
    })?;
    // Effective arm time: when `--start` (journaling replay) is given, record
    // the plan *as if* it were armed at that cursor, not at the wall-clock run
    // time — so a replayed arming reads back the historical moment. Otherwise
    // use the real `now`.
    let armed_at = effective_arm_time(replay_start, now);
    // Pullback prep (--pull-back): capture the arm-time anchor (live mid) and the
    // ATR multiple so `build_trade_plan` can bake them onto the trigger. Read only
    // when a pullback is armed. A live-mid read failure is fatal — a bad/guessed
    // anchor would silently mis-fire every pullback (same discipline as the M/W
    // arm-time spread read).
    let pullback_arm = match built_trade.spec.pull_back {
        Some(atr_mult) => {
            let broker = built_trade.spec.broker;
            let anchor_open = read_mid_blocking(kind_to_broker(broker), &built_trade.instrument)
                .wrap_err("read live mid for --pull-back anchor")?;
            Some(crate::trade_plan_build::PullbackArm {
                anchor_open,
                atr_mult,
            })
        }
        None => None,
    };
    let mut plan = build_trade_plan(
        &built_trade.trade_id,
        &built_trade.instrument,
        &built_trade.alerts,
        direction,
        geom,
        granularity,
        is_mw,
        shadow,
        replay_start,
        retest_atr_step,
        cross_buffer_pct,
        cross_buffer_atr,
        bcr_require_golden,
        armed_at,
        armed_sentiment,
        pullback_arm,
    );
    // Unwrap the tv-arm bundle wrappers to the cli `BuiltPause`/`BuiltNews` the
    // appender reads (each carries the signed intents + window times).
    let pauses: Vec<&cli::BuiltPause> = pause_bundles.iter().map(|b| &b.built).collect();
    let newses: Vec<&cli::BuiltNews> = news_bundles.iter().map(|b| &b.built).collect();
    append_control_rules(&mut plan, &pauses, &newses);
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
    // M/W resolution moved to `crate::mw_resolve`; these tests exercise it
    // directly. Imported HERE rather than at file scope so the non-test build
    // doesn't carry unused imports.
    use crate::calendar::calendar_scope_range;
    use crate::hs_resolve::{
        build_trade_spec, check_prep_expiries, hs_entry_level_vetos, prep_expiry_steps,
    };
    use crate::mw_resolve::{MwSpecAnchors, build_mw_trade_spec};
    use crate::news_window::NewsWindow;
    use crate::test_drawings::{fib, now, path_n, two_point, vline};
    use chrono::TimeZone;
    use clap::Parser;

    /// `arm_from_inputs` must make **no chart calls**. That is the entire point
    /// of the split — everything below the seam has to work identically whether
    /// its `SetupInputs` came from TradingView or from a frozen file.
    ///
    /// This scans the source rather than relying on types, because the property
    /// is *transitive*: `arm_from_inputs` holds no `TvMcp`, so the compiler
    /// already stops a direct call — but nothing stops a future edit from
    /// calling a **helper** that constructs its own `TvMcp::new(...)` internally
    /// and reaches the chart that way. The type system can't express "and
    /// nothing you call does it either"; a scan can.
    ///
    /// Deliberately a scan of the function body's own text, not a grep of the
    /// whole file: the chart half legitimately contains all of these.
    #[test]
    fn arm_from_inputs_makes_no_chart_calls() {
        let src = include_str!("pipeline.rs");
        let start = src
            .find("\nfn arm_from_inputs(")
            .expect("arm_from_inputs must exist — did it get renamed?");
        // The body ends at the next column-0 `}`.
        let rest = &src[start + 1..];
        let end = rest.find("\n}\n").map_or(rest.len(), |i| i + 2);
        let body = &rest[..end];

        for banned in [
            "TvMcp",
            "get_state(",
            "get_range(",
            "list_drawings(",
            "get_symbol_info(",
            "draw_news_markers(",
            "classify(",
            "calendar_windows(",
        ] {
            // Skip comment lines — one legitimately mentions the retired tv-mcp
            // alert path in prose.
            let hit = body
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .any(|l| l.contains(banned));
            assert!(
                !hit,
                "arm_from_inputs (or its inlined body) references {banned:?} — the \
                 chart/plan seam is broken, and a frozen-spec arm would try to reach \
                 TradingView"
            );
        }
        // Sanity: the slice really is the function body, not an empty string —
        // otherwise every assertion above passes vacuously.
        assert!(
            body.contains("build_trade_from_spec"),
            "the extracted body doesn't look like arm_from_inputs; the scan would \
             have passed vacuously"
        );
    }

    #[test]
    fn effective_arm_time_uses_wallclock_without_start() {
        // No `--start` → the plan is armed at the real run time.
        assert_eq!(effective_arm_time(None, now()), now());
    }

    #[test]
    fn effective_arm_time_uses_start_cursor_when_given() {
        // `--start` present → record the plan as if armed at that historical
        // cursor, not the wall-clock run time.
        let cursor = Utc.with_ymd_and_hms(2026, 5, 1, 9, 30, 0).unwrap();
        let armed = effective_arm_time(Some(cursor.timestamp()), now());
        assert_eq!(armed, cursor);
        assert_ne!(armed, now());
    }

    fn wallclock(at: DateTime<Utc>) -> AsOf {
        AsOf::wallclock(at)
    }

    /// A `NewsWindow` from two unix-second boundaries — the calendar-resolved
    /// replacement for the old `(vline, vline)` drawn pair in these tests.
    fn nw(start_unix: i64, end_unix: i64) -> NewsWindow {
        NewsWindow::new(
            DateTime::<Utc>::from_timestamp(start_unix, 0).expect("valid start"),
            DateTime::<Utc>::from_timestamp(end_unix, 0).expect("valid end"),
        )
    }

    // ===== control bundles ==============================================

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
        let err = check_prep_expiries(&PlanGeometry::from_roles(&roles), now()).unwrap_err();
        assert!(err.contains("break-and-close-expiry"), "msg = {err}");
        assert!(err.contains("never enter"), "msg = {err}");
    }

    #[test]
    fn prep_expiry_future_with_prep_ok() {
        // Same future cutoff, but the break-and-close line is present →
        // a legitimate "pattern got too big" cutoff. No error.
        //
        // The neckline must be a genuine TWO-point trendline. This fixture used a
        // one-point `vline`, which the old presence check
        // (`roles.break_and_close.is_some()`) happily accepted — even though
        // plan-building would then produce NO `TrendlineCross` rule for it
        // (`points.get(1)?` → `None`). So the old check could pass a setup that
        // could never enter. Reading presence off `geom.neckline`, which requires
        // both anchors, makes the guard agree with what plan-building needs.
        let roles = Roles {
            break_and_close: Some(two_point("neck", "neckline", 1.10, 1.09)),
            prep_expiries: vec![(
                "break-and-close".into(),
                vline("e", now().timestamp() + 3600),
            )],
            ..Default::default()
        };
        check_prep_expiries(&PlanGeometry::from_roles(&roles), now()).unwrap();
    }

    #[test]
    fn prep_expiry_in_past_is_warn_not_error() {
        // Cutoff already lapsed → we're re-arming later in time; warn
        // only, even with no prep drawing present.
        let roles = Roles {
            prep_expiries: vec![("retest".into(), vline("e", now().timestamp() - 3600))],
            ..Default::default()
        };
        check_prep_expiries(&PlanGeometry::from_roles(&roles), now()).unwrap();
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
        assert_eq!(
            prep_expiry_steps(&PlanGeometry::from_roles(&roles), &[]),
            vec!["break-and-close", "retest"]
        );
    }

    #[test]
    fn prep_expiry_steps_drops_skipped_preps() {
        // A `break-and-close-expiry` line is drawn, but `--quasimodo` puts
        // `break-and-close` in skip_preps — there's no prep to expire, so the
        // step must be filtered out (else the CLI validator rejects the same
        // name appearing in both skip_preps and prep_expiries).
        let roles = Roles {
            prep_expiries: vec![
                ("break-and-close".into(), vline("a", 1)),
                ("retest".into(), vline("b", 2)),
            ],
            ..Default::default()
        };
        let skip = vec!["break-and-close".to_string()];
        assert_eq!(
            prep_expiry_steps(&PlanGeometry::from_roles(&roles), &skip),
            vec!["retest"]
        );
    }

    // ===== calendar news-scope range ====================================

    #[test]
    fn calendar_scope_range_is_cursor_to_expiry() {
        // With an expiry, the range is [cursor, expiry] verbatim — the chart's
        // visible area plays no part.
        let cursor = now().timestamp();
        let expiry = now() + chrono::Duration::hours(30);
        let range = calendar_scope_range(cursor, Some(expiry));
        assert_eq!(range, (cursor, expiry.timestamp()));
    }

    #[test]
    fn calendar_scope_range_without_expiry_is_empty() {
        // No resolved expiry → right edge collapses to the cursor, giving an
        // empty [t, t] range so `calendar_windows` yields nothing (rather than
        // fetching across all of time).
        let cursor = now().timestamp();
        let range = calendar_scope_range(cursor, None);
        assert_eq!(range, (cursor, cursor));
        assert!(range.1 <= range.0, "empty range: to <= from");
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
            &mw_args(&["plan-out", "/tmp/x.json"]),
            now,
            cursor.timestamp(),
            None,
        );

        assert_eq!(as_of.at, cursor);
        assert_eq!(as_of.source, "replay-cursor");

        // …and that as-of actually reaches the prune: an event 12h after the
        // cursor (still in the past vs `now`) survives. This is the integration
        // half — `ControlWindows` owns and tests the prune rule itself; what
        // matters here is that the *selected* yardstick is the one it's handed.
        let event_end = cursor.timestamp() + 12 * 3600;
        let control =
            ControlWindows::new(vec![nw(event_end - 1800, event_end)], vec![], vec![], as_of);
        assert_eq!(
            control.blackout().len(),
            1,
            "upcoming-vs-cursor window kept"
        );
    }

    /// Live arm: `--register-plan` always prunes against wall-clock now, even
    /// though the chart cursor (a tightly-zoomed live view) may sit in the past.
    #[test]
    fn pick_prune_as_of_register_plan_uses_wallclock() {
        let now = now();
        let cursor = "2026-05-28T21:00:00Z".parse::<DateTime<Utc>>().unwrap();

        let as_of = pick_prune_as_of(&mw_args(&["register"]), now, cursor.timestamp(), None);

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
            &mw_args(&["plan-out", "/tmp/x.json"]),
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
            &mw_args(&["--as-of", "2026-05-28T21:00:00Z", "plan-out", "/tmp/x.json"]),
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
            &mw_args(&["--as-of", "not-a-date", "plan-out", "/tmp/x.json"]),
            now,
            cursor.timestamp(),
            None,
        );

        assert_eq!(as_of.at, cursor);
        assert_eq!(as_of.source, "replay-cursor");
    }

    // ===== --spec-in ====================================================

    /// A frozen arm refuses the position tools **at runtime**, not just at the
    /// clap layer.
    ///
    /// The clap `conflicts_with` catches an operator typing both flags, but it
    /// is not the invariant: anything that builds `Args` directly — which every
    /// test in this crate does, and which a future caller might — bypasses clap
    /// entirely. Without this guard a frozen arm would reach
    /// `run_position_entry` with `roles: None` and arm a *different trade* off
    /// whatever the pattern path produced.
    ///
    /// Found by mutation: replacing the guard's condition with `false` left all
    /// 314 tests green, so nothing was actually checking it.
    #[test]
    fn spec_in_refuses_the_position_tools_even_when_clap_is_bypassed() {
        let spec = crate::frozen_setup::FrozenSetup::capture(
            PlanGeometry::default(),
            "60".into(),
            "OANDA:EUR_USD".into(),
            Some(1_700_000_000),
            None,
        );
        let path = std::env::temp_dir().join(format!("spec-refuse-{}.json", std::process::id()));
        spec.write(&path).expect("write spec");

        // Built directly, exactly as a non-clap caller would — `market_entry`
        // and `spec_in` both set, which clap would have rejected.
        let mut args = mw_args(&[]);
        args.market_entry = true;
        args.spec_in = Some(path.clone());

        let err = read_setup_from_spec(&args, &path)
            .expect_err("a frozen arm has no drawn position tool")
            .to_string();
        assert!(
            err.contains("position") && err.contains("--spec-in"),
            "the error must say WHY, so the operator knows to arm off the chart: {err}"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A spec with no cursor and no `--start` is refused rather than silently
    /// defaulting to wall-clock "now".
    ///
    /// Defaulting would be wrong in the one case that matters: re-arming a
    /// historical setup for the corpus. It'd prune every news window as elapsed
    /// and score the trade against today's calendar instead of its own.
    #[test]
    fn a_cursorless_spec_without_start_is_refused() {
        let spec = crate::frozen_setup::FrozenSetup::capture(
            PlanGeometry::default(),
            "60".into(),
            "OANDA:EUR_USD".into(),
            None, // no cursor
            None,
        );
        let path = std::env::temp_dir().join(format!("spec-nocursor-{}.json", std::process::id()));
        spec.write(&path).expect("write spec");

        let err = read_setup_from_spec(&mw_args(&[]), &path)
            .expect_err("no cursor anywhere")
            .to_string();
        assert!(err.contains("no cursor"), "err = {err}");

        // …and supplying --start resolves it.
        let args = mw_args(&["--start", "2026-06-20T17:00"]);
        assert!(
            read_setup_from_spec(&args, &path).is_ok(),
            "--start must supply the missing cursor"
        );
        std::fs::remove_file(&path).ok();
    }

    // ===== M / W trade-spec resolution ==================================

    fn mw_args(extra: &[&str]) -> Args {
        let mut argv = vec!["tv-arm"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).expect("parse mw args")
    }

    /// `--start` accepts the same forms `replay-candles --start` does.
    ///
    /// This used to call `DateTime::parse_from_rfc3339` directly, which demands
    /// **seconds** and rejects a bare local time — so
    /// `--start 2026-06-19T17:00+10:00` was a hard error on `tv-arm` while
    /// working fine on `replay-candles`, even though `tv-arm ... replay`
    /// forwards that exact string to it. The error read like a malformed
    /// timestamp rather than like the two tools disagreeing.
    #[test]
    fn start_accepts_the_forms_replay_candles_accepts() {
        // All five spell the same instant: 17:00 Brisbane == 07:00 UTC.
        let want = parse_start(&mw_args(&["--start", "2026-06-20T17:00:00+10:00"]))
            .expect("the one form raw RFC3339 also accepts");
        assert!(want.is_some());
        for s in [
            "2026-06-20T17:00",       // bare local, minute precision
            "2026-06-20T17:00:00",    // bare local, seconds
            "2026-06-20T17:00+10:00", // offset, no seconds
            "2026-06-20T07:00Z",      // Z, no seconds
        ] {
            assert!(
                DateTime::parse_from_rfc3339(s).is_err(),
                "{s:?} parses as raw RFC3339 — this test's premise is stale"
            );
            assert_eq!(
                parse_start(&mw_args(&["--start", s])).expect(s),
                want,
                "disagreement on {s:?}"
            );
        }
    }

    /// A bare datetime is **Brisbane**, not UTC. Getting this wrong shifts the
    /// journaling cursor by 10 hours — enough to arm against a different
    /// session entirely, with no error anywhere.
    #[test]
    fn a_bare_start_is_brisbane_not_utc() {
        let bne = parse_start(&mw_args(&["--start", "2026-06-20T17:00"])).expect("bare");
        let utc = parse_start(&mw_args(&["--start", "2026-06-20T17:00Z"])).expect("Z");
        assert_ne!(bne, utc, "a bare time must not be read as UTC");
        assert_eq!(
            bne.zip(utc).map(|(b, u)| b - u),
            Some(-10 * 3600),
            "Brisbane is UTC+10, so the same clock face is 10h EARLIER in UTC terms"
        );
    }

    #[test]
    fn start_absent_is_none_and_garbage_is_an_error() {
        assert_eq!(parse_start(&mw_args(&[])).expect("no flag"), None);
        assert!(parse_start(&mw_args(&["--start", "yesterday"])).is_err());
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
            &PlanGeometry::default(),
            false,
            0.01,
            // Distinct tick (finer than pip) to prove it's baked independently.
            0.001,
            Vec::new(),
        );
        assert_eq!(spec.pattern, cli::TradePattern::Hs);
        assert!(spec.mw.is_none());
        assert_eq!(spec.pip_size, Some(0.01));
        assert_eq!(spec.tick_size, Some(0.001));
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
            &PlanGeometry::default(),
            false,
            0.0001,
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
            &PlanGeometry::default(),
            false,
            0.0001,
            0.0001,
            Vec::new(),
        );
        assert!(
            !skipped.needs_golden,
            "--skip-golden must clear needs_golden on the spec"
        );
    }

    /// `close_on_news` reaches the spec verbatim from the caller's news fact.
    ///
    /// Previously this was derived inside `build_trade_spec` from a field on
    /// `Roles` and no test covered it at all — a silent gap on the flag that
    /// decides whether an open position gets flattened around a news release.
    /// Now that the news windows live in `ControlWindows`, the fact is an
    /// argument, so it's directly testable: assert both polarities so a
    /// hard-coded `false` (or an inverted one) fails.
    #[test]
    fn close_on_news_is_carried_onto_the_spec_both_ways() {
        let spec_for = |close_on_news| {
            build_trade_spec(
                &mw_args(&[]),
                "EUR_USD",
                "ms-oanda-1",
                Broker::Oanda,
                Direction::Long,
                now() + chrono::Duration::days(1),
                1.05,
                &PlanGeometry::default(),
                close_on_news,
                0.0001,
                0.0001,
                Vec::new(),
            )
        };
        assert!(
            spec_for(true).close_on_news,
            "a trade with a news window in its lifetime must close on news"
        );
        assert!(!spec_for(false).close_on_news, "…and one without must not");
    }

    /// The wiring the above can't see: `ControlWindows::has_news` is what the
    /// pipeline actually passes, and it must read the PRUNED set. An all-elapsed
    /// calendar would otherwise arm a news close for a window that has already
    /// finished — arming a guard against an event that cannot recur.
    #[test]
    fn has_news_drives_close_on_news_from_the_pruned_window_set() {
        let t = now().timestamp();
        let as_of = wallclock(now());

        let all_elapsed = ControlWindows::new(vec![], vec![nw(t - 7200, t - 3600)], vec![], as_of);
        assert!(
            !all_elapsed.has_news(),
            "every news window elapsed → no close_on_news"
        );

        let live = ControlWindows::new(vec![], vec![nw(t + 60, t + 3600)], vec![], as_of);
        assert!(live.has_news(), "a live news window → close_on_news");
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
            &PlanGeometry::default(),
            false,
            0.0001,
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
            &PlanGeometry::from_roles(&Roles::default()),
            trade_control_core::broker::Granularity::H1,
            false,
            false,
            None,
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            args.bcr_require_golden,
            chrono::Utc::now(),
            None,
            None, // pullback_arm
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
    fn bcr_require_golden_flag_bakes_onto_emitted_plan() {
        // `--bcr-require-golden` flips the plan-level `bcr_require_golden` to
        // true on the emitted plan; default (absent) is false. This is the
        // break/retest candle-quality gate — distinct from `--skip-golden`,
        // which governs the enter's Pine signal bar (`needs_golden`).
        let default = emitted_plan_with(&["--skip-break-and-close", "--skip-retest"]);
        assert!(
            !default.bcr_require_golden,
            "bcr_require_golden defaults to false (off)"
        );
        let on = emitted_plan_with(&[
            "--skip-break-and-close",
            "--skip-retest",
            "--bcr-require-golden",
        ]);
        assert!(
            on.bcr_require_golden,
            "--bcr-require-golden must set bcr_require_golden: true on the plan"
        );
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
    fn strategy_v2_default_qm_leg_is_a_limit_bcr_stays_a_stop() {
        // Default (no --qm-entry): the QM leg (09-enter-qm) rests as a LIMIT at
        // the signal level (recover→stop when wrong-side); the BCR leg
        // (05-enter) stays a STOP. `--qm-entry stop`/`market` override the QM
        // leg only.
        use trade_control_core::intent::{Action, EntrySpec};
        let plan = emitted_plan_with(&["--strategy-v2"]);
        let json = serde_json::to_string_pretty(&plan).unwrap();

        let qm = plan
            .rules
            .iter()
            .find(|r| r.intent.action == Action::Enter && r.rule_id.contains("enter-qm"))
            .unwrap_or_else(|| panic!("no QM enter rule in plan\n{json}"));
        let bcr = plan
            .rules
            .iter()
            .find(|r| r.intent.action == Action::Enter && !r.rule_id.contains("enter-qm"))
            .unwrap_or_else(|| panic!("no BCR enter rule in plan\n{json}"));

        assert!(
            matches!(qm.intent.entry, Some(EntrySpec::Limit { .. })),
            "QM leg should default to a Limit, got {:?}",
            qm.intent.entry
        );
        assert!(
            matches!(bcr.intent.entry, Some(EntrySpec::Stop { .. })),
            "BCR leg should stay a Stop, got {:?}",
            bcr.intent.entry
        );
    }

    #[test]
    fn mw_spec_never_needs_golden() {
        // Golden is an H&S signal-candle gate; M/W entry is geometry-driven, so
        // the M/W spec carries needs_golden: false regardless of --skip-golden.
        let anchors = || MwSpecAnchors {
            runup_start: 1.0500,
            first_point: 1.1000,
            neckline: 1.0800,
            right_shoulder: None,
            spread_pips: 1.0,
            pip_size: 0.0001,
            tick_size: 0.0001,
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
        assert!(!default.needs_golden, "M/W never gates on golden");

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
            "M/W stays golden-free with --skip-golden too"
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
            &PlanGeometry::default(),
            false,
            pip_size,
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
            // fib: head 1.1000 above neckline 1.0900 (short).
            tp_fib: Some(fib("fib", 1.1000, 1.0900)),
            // invalidation horizontal (1 price).
            invalidation: Some(path_n("inv", &[1.1050])),
            ..Default::default()
        };
        let vetos = hs_entry_level_vetos(&PlanGeometry::from_roles(&roles), Direction::Short);
        let by = |n: &str| vetos.iter().find(|v| v.name == n).expect("present");
        // pcl = fib 1.8 = neckline + 0.8×(TP − neckline).
        //   tp = 2×1.0900 − 1.1000 = 1.0800,
        //   level = 1.0900 + 0.8×(1.0800 − 1.0900) = 1.0820.
        let low = by("too-low");
        assert_eq!(low.past, VetoSide::Below);
        assert!((low.level - 1.0820).abs() < 1e-9, "{}", low.level);
        let high = by("too-high");
        assert_eq!(high.past, VetoSide::Above);
        assert!((high.level - 1.1050).abs() < 1e-9);

        // Missing fib → only the invalidation veto is baked (NaN is skipped).
        roles.tp_fib = None;
        let vetos = hs_entry_level_vetos(&PlanGeometry::from_roles(&roles), Direction::Short);
        assert_eq!(vetos.len(), 1);
        assert_eq!(vetos[0].name, "too-high");
    }

    #[test]
    fn hs_entry_level_vetos_long_mirrors() {
        // IH&S long: sides flip. pcl named too-high/Above, invalidation
        // too-low/Below.
        use trade_control_core::intent::VetoSide;
        let roles = Roles {
            tp_fib: Some(fib("fib", 1.0900, 1.1000)), // head 1.0900 below neckline 1.1000 (long)
            invalidation: Some(path_n("inv", &[1.0850])),
            ..Default::default()
        };
        let vetos = hs_entry_level_vetos(&PlanGeometry::from_roles(&roles), Direction::Long);
        let pcl = vetos.iter().find(|v| v.name == "too-high").expect("pcl");
        assert_eq!(pcl.past, VetoSide::Above);
        let inv = vetos.iter().find(|v| v.name == "too-low").expect("inv");
        assert_eq!(inv.past, VetoSide::Below);
        assert!((inv.level - 1.0850).abs() < 1e-9);
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
