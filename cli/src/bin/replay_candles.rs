//! `replay-candles` — replay a historical candle window through the cron
//! engine's pure decision logic, offline.
//!
//! Given a `TradePlan` (written by `tv-arm --plan-out`) and a time window, this
//! pulls the broker candles for that window (via candle-cache), feeds them
//! through the engine's `evaluate_plan` one closed bar at a time exactly as the
//! live cron tick does, and — for each fired enter — runs the pure
//! `simulate_fill` over the forward candles to show what the broker would have
//! done. No `wrangler dev`, no HTTP, no live broker orders.
//!
//! The worker has no candle-ingest endpoint and its order-dispatch path
//! (`run_enter`) can't run off-wasm (it builds a `worker::Response` that panics
//! at construction), so this drives the *pure* engine core natively and uses the
//! fill simulator as the faithful, broker-free stand-in for execution.
//!
//! With explicit flags:
//!
//! ```text
//! replay-candles --plan plan.json --instrument eur/cad --granularity 1h \
//!   --source tradenation --start 2026-06-18T11:00
//! ```
//!
//! Or, with no window flags, the window resolves itself from the plan + the
//! live TradingView chart, for the natural replay workflow:
//!
//!   - **granularity** comes from the **plan** (`plan.granularity`).
//!   - **start** is the plan's baked `replay_start` (from `tv-arm --start`) when
//!     present, else the chart's **last shown candle** (`bars_range.to`) — in TV
//!     replay mode that's the replay cursor, so an arm without `--start` still
//!     works by rewinding the chart to the trade start. A plan armed *with*
//!     `--start` carries its own cursor, so the chart position no longer matters.
//!   - **end** is the plan's **trade-expiry** rule (`TimeReached.at_epoch`),
//!     falling back to the chart's visible-region end if the plan has none.
//!   - **instrument** falls back chart-symbol → plan.
//!
//! So the operator rewinds TradingView to the trade start and just runs
//! `replay-candles --plan plan.json`. Any flag that *is* passed overrides the
//! corresponding resolved value.

mod replay_candles {
    pub mod annotate;
    pub mod arm_record;
    pub mod baseline;
    pub mod batch;
    pub mod brisbane;
    pub mod candles;
    pub mod economics;
    pub mod fill_sim;
    pub mod fixture;
    pub mod golden_eq;
    pub mod granularity;
    pub mod instrument;
    pub mod lazy_zoom;
    pub mod lifecycle;
    pub mod outcome;
    pub mod replay;
    pub mod replay_broker;
    pub mod report;
    pub mod sentiment;
    pub mod spread_breakdown;
    pub mod tv;
    pub mod verbose;
}

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, TimeZone, Utc};
use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use color_eyre::eyre::{Context, Result, eyre};
use tracing_error::ErrorLayer;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

use replay_candles::arm_record;
use replay_candles::baseline;
use replay_candles::batch;
use replay_candles::fixture::{self, FixtureMeta, ReplayOutcome};
use replay_candles::tv::TvDefaults;
use replay_candles::{
    annotate, brisbane, candles, economics, golden_eq, granularity, instrument, lazy_zoom, outcome,
    replay, report, sentiment, tv,
};
use trade_control_cli::replay_args::{CandleSource, DetectorMarkConfig, ReplayArgs as Args};
use trade_control_engine::{BidAskCandle as EngineCandle, Granularity, TradePlan, Trigger};
use trading_view::mcp::TvMcp;

/// Classify whatever [`run`] returned, always print a terminal line, and exit
/// with a code that tells a driver what to do about it.
///
/// `main` deliberately returns `()` rather than `Result`: letting eyre print
/// and exit(1) makes every failure look the same, and — worse — prints
/// *nothing* on the summary line a batch driver scrapes. A run that died at
/// startup was then indistinguishable from one that ran and found no trade.
/// See `replay_candles::outcome`.
#[tokio::main]
async fn main() {
    // `--json` owns stdout: the failure line below would append trailing text to
    // the JSON document and make it unparseable, which is the *same* ambiguity
    // this machinery exists to remove — a driver that can't parse the output is
    // back to guessing. Under `--json` the failure is already reported IN the
    // JSON (`ok: false` + `error` per row, `failed` in the roll-up), so the text
    // line is redundant as well as harmful. Sniffed from raw argv because a
    // failure can predate the clap parse.
    //
    // Suppressing the line is only safe because `--json` now `requires =
    // "test_mode"` and BOTH test-mode paths print their JSON *before* returning
    // the error that sets the exit code (`run_test_mode_single` at the `--check`
    // step, `run_test_mode_batch` before its `failed > 0` raise). So "exit != 0
    // with `--json`" still comes with a document on stdout. If you add a
    // test-mode failure that can return `Err` *before* its `println!`, this
    // suppression turns it into zero bytes — emit a row there instead.
    let json_mode = std::env::args().any(|a| a == "--json");
    match run().await {
        Ok(()) => std::process::exit(outcome::EXIT_OK),
        Err(report) => {
            let kind = outcome::FailureKind::classify(&report);
            // The human-readable chain first (stderr), then the machine-readable
            // terminal line (stdout, where the summary it mirrors goes).
            eprintln!("Error: {report:?}");
            if !json_mode {
                println!(
                    "{}",
                    outcome::FailureLine {
                        kind,
                        detail: &report.to_string(),
                    }
                );
            }
            std::process::exit(kind.exit_code());
        }
    }
}

async fn run() -> Result<()> {
    color_eyre::install()?;

    // Handle completions before clap's required-arg validation: `--plan` is
    // required, so a plain `Args::parse()` would reject a bare
    // `--print-completions`. Detect it on the raw argv first, emit, and exit.
    if std::env::args().any(|a| a == "--print-completions") {
        print_completions();
        return Ok(());
    }

    init_tracing();

    let args = Args::parse();

    // `--test-mode` is a fully-offline branch: no broker, no TradingView, no
    // env vars — everything comes from the saved fixture.
    if args.test_mode {
        return run_test_mode(&args).await;
    }

    // `--annotate-unfilled` is a superset of `--annotate` (it adds the
    // not-taken trades), so it implies annotation is on.
    let annotate = args.annotate || args.annotate_unfilled;
    // Annotation draws each position, which needs the simulated fill — so
    // annotating forces simulation on even if the operator passed
    // `--simulate false`.
    let simulate = args.simulate || annotate;
    if annotate && !args.simulate {
        tracing::info!("annotation implies --simulate; running the fill simulator");
    }

    let plan_path = args.plan.clone().ok_or_else(|| {
        outcome::bad_input(eyre!(
            "--plan is required (or use --test-mode --fixture <name>)"
        ))
    })?;
    let plan = load_plan(&plan_path)?;

    // Granularity comes from the plan; `--granularity` only overrides, and an
    // override must still match the plan (a mismatch would replay the wrong
    // bars). `gran_label` is the friendly form for logging / errors.
    let gran = resolve_granularity(&args, &plan)?;
    let gran_label = granularity::engine_label(plan.granularity);

    let window = resolve_window(&args, &plan)?;

    let raw_instrument = window.instrument.as_deref().unwrap_or(&plan.instrument);
    let symbol = instrument::resolve_for(raw_instrument, args.source)?;

    let start = window.start;
    let end = window.end;
    if end <= start {
        return Err(outcome::bad_input(inverted_window_error(&window)));
    }

    // The engine evaluates a `TimeReached` (trade-expiry) against each candle's
    // *open* time, so a trade-expiry at `end` only fires once a bar *opens* at
    // or after `end` — one bar past it. Pull that extra bar so the expiry
    // actually fires (without it the window stops one bar short and the plan
    // never retires). Harmless when there's no expiry: the engine stops at the
    // first `done`, and trailing candles are ignored.
    let pull_end_raw = end + Duration::seconds(gran.engine().seconds());
    // Clamp to the last *closed* bar: a plan whose trade-expiry is still in the
    // future asks for candles that don't exist yet, and OANDA rejects a request
    // whose `from` lands in the future ("Invalid value specified for 'from'.
    // Time is in the future") — the request optimizer's gap-fill chunk for the
    // not-yet-printed tail starts at a future `from`. There's nothing to replay
    // past now anyway — the engine only sees bars that have printed. Snap the
    // pull end back to one bar before now, so we only ever request fully closed
    // bars and never the current still-forming one.
    let last_closed = Utc::now() - Duration::seconds(gran.engine().seconds());
    let pull_end = pull_end_raw.min(last_closed);

    // Pull a silent warm-up prefix before `start`: these bars seed the detector
    // (warm ATR, pattern context) and the FSM but fire nothing — the plan goes
    // live at `start` (see `replay::run`'s `live_start`). `warmup_bars` is a
    // count of *real* candles we want before `start`, not a wall-clock span —
    // that distinction is load-bearing. The cache pull is time-windowed, so we
    // start from a time estimate (`warmup_bars × bar`) but a **market gap**
    // (weekend, session close) inside that span yields fewer real candles than
    // wall-time bars. A crypto CFD on TradeNation, say, gaps Fri→Sun, so 200
    // bars of wall-time over a weekend can return ~18 candles — far short of the
    // 96-bar M15 ATR, leaving ATR `na` for the whole replay so a `needs_golden`
    // enter never fires (the "not seeing the entry candle" bug). So we pull,
    // count the real pre-`start` candles, and if short, widen the look-back and
    // retry — hopping the gap — until we have enough or hit a back-off cap.
    let bar_secs = gran.engine().seconds();
    // Floor the warmup at the SHARED detector-lookback depth (the same
    // `detector_lookback_bars` the live worker's `pine_lookback_since` uses), so
    // the replay's detector window can never be shallower than live's — the two
    // ATR-warmup depths are one number, and a low `--warmup-bars` can't silently
    // re-introduce the golden-starvation divergence. The default (200) already
    // clears every ATR length; this only bites when an operator lowers it.
    let warmup_bars = {
        use trade_control_core::signals::{DetectorConfig, detector_lookback_bars};
        let cfg = DetectorConfig::pine_defaults(gran.engine());
        args.warmup_bars
            .max(detector_lookback_bars(&cfg, gran.engine()))
    };
    let candles = pull_with_warmup(
        args.source,
        &symbol,
        gran,
        gran_label,
        start,
        pull_end,
        bar_secs,
        warmup_bars,
        args.cache_dir.clone(),
    )
    .await?;

    // The candle-detector mark config, from the two `--candle-detector-*` flags,
    // relative to the plan's trade direction (the `with`/`against` reference).
    let mark_cfg = DetectorMarkConfig::new(
        args.candle_detector_direction,
        args.candle_detector_golden,
        plan.direction,
    );

    // Market-hours blackout is no longer resolved here: `run_enter`'s reject gate
    // and `sweep_reason` both read the baked, weekday-aware mask keyed on the
    // instrument (`core::intent::market_hours_blocked`), so there is no
    // `market_info` fetch and no store seed in the replay path anymore.

    // Keep the state TTL past the window so nothing expires mid-replay.
    let expires_at = end + Duration::days(365);

    // Sub-bar zoom (PR-2), run LAZILY in two passes (see `lazy_zoom`).
    //
    // PASS 1: replay with a recorder that serves no finer candles — behaviourally
    // identical to `NoZoom`, so this pass keeps the pessimistic stop — while
    // recording the sub-windows the sim asked for. Those are exactly the bars
    // that straddle both SL and TP, i.e. the only bars finer candles can change.
    //
    // Why not the old eager pull: it fetched a finer series across the WHOLE
    // coarse window up front, but the exit loop returns on the FIRST ambiguous
    // bar, so each entry zooms at most once. On the CAD/SGD H1 fixture that was
    // 11,160 M1 slots pulled to disambiguate at most 2 bars — and a cell with no
    // entry pulled the same 11k and consulted none of it. Those M1 slots are also
    // where candle-cache re-fetches every run (an illiquid cross has scattered
    // minutes that never ticked, which a partial broker response leaves
    // unrecorded), so the eager pull paid that cost on every replay.
    // `Rc` so the driver still holds the recorder after `run` takes ownership of
    // its `SubBars` box (the box is `Rc<RecordingSubBars>`, which impls the trait
    // by delegation) — that's how the recorded windows come back out.
    let recorder = std::rc::Rc::new(lazy_zoom::RecordingSubBars::new());
    let pass1 = replay::run(
        &plan,
        &candles,
        gran.engine(),
        start,
        expires_at,
        mark_cfg,
        Some(Box::new(std::rc::Rc::clone(&recorder))),
    )
    .await;

    let zoom_windows = recorder.windows();

    // The finer candles the zoom actually consulted, kept so `--save` can freeze
    // exactly them into the fixture (and nothing more).
    let mut zoom_bars: Vec<EngineCandle> = Vec::new();

    // No ambiguous bar ⇒ no finer candles can change the outcome ⇒ pass 1 IS the
    // answer and we fetch nothing at all. This is the common case.
    let replay = if zoom_windows.is_empty() {
        tracing::info!("sub-bar zoom: no ambiguous SL/TP bars — no finer candles fetched");
        pass1
    } else {
        match granularity::finer(gran.engine()) {
            // Plan already at the finest grain — nothing to zoom into, so pass 1
            // stands (its pessimistic stop is the floor).
            None => pass1,
            Some(finer_gran) => {
                // Fetch ONLY the recorded windows, then replay again with them.
                let fetched = lazy_zoom::fetch_windows(
                    args.source,
                    &symbol,
                    finer_gran,
                    &zoom_windows,
                    args.cache_dir.clone(),
                )
                .await;

                if fetched.is_empty() {
                    // Every window failed or served nothing — fail-soft exactly as
                    // the eager pull did: keep pass 1's pessimistic stop.
                    tracing::warn!(
                        finer = granularity::engine_label(finer_gran.engine()),
                        windows = zoom_windows.len(),
                        "sub-bar zoom: no finer candles for the ambiguous bars — \
                         falling back to pessimistic stop"
                    );
                    pass1
                } else {
                    tracing::info!(
                        finer = granularity::engine_label(finer_gran.engine()),
                        windows = zoom_windows.len(),
                        bars = fetched.len(),
                        "sub-bar zoom: fetched finer candles for the ambiguous bars only"
                    );
                    zoom_bars = fetched.clone();
                    // PASS 2: same plan, same coarse candles, same seed — the only
                    // difference is that ambiguous bars can now be resolved.
                    replay::run(
                        &plan,
                        &candles,
                        gran.engine(),
                        start,
                        expires_at,
                        mark_cfg,
                        Some(Box::new(lazy_zoom::WindowSubBars::new(fetched))),
                    )
                    .await
                }
            }
        }
    };

    // Recompute the news-sentiment verdict for the replay window (same algorithm
    // tv-news / tv-arm use), as of the plan's armed_at or the window start.
    // Fail-soft — `None` on any miss, and the report simply omits the block.
    let replay_sentiment = sentiment::resolve_replay_sentiment(&plan, start).await;

    // Render once and keep the economics it booked: `--save` records them into
    // the fixture's `expected.json`, so the printed `Net R:` and the saved golden
    // are the same computation.
    let rendered = report::render(
        &plan,
        &replay,
        simulate,
        args.verbose,
        replay_sentiment.as_ref(),
        &mark_cfg,
    );
    print!("{}", rendered.text);

    if annotate {
        let mcp = match &args.tv_mcp_root {
            Some(root) => TvMcp::new(root.clone()),
            None => TvMcp::default(),
        };
        let scope = if args.annotate_unfilled {
            "positions (incl. not-taken)"
        } else {
            "filled positions"
        };
        tracing::info!(root = %mcp.root().display(), "annotating {scope} on the chart");
        let drawn = annotate::annotate(&mcp, &plan, &replay, args.annotate_unfilled)?;
        println!("annotated {drawn} position(s) on the chart");
    }

    if let Some(name) = &args.save {
        let meta = FixtureMeta {
            instrument: symbol.clone(),
            granularity: gran.engine(),
            source: args.source,
            start,
            end,
            message: args.message.clone(),
            arm: Some(arm_record(&args)),
        };
        let expected = ReplayOutcome::compute(&replay, simulate, Some(&rendered.economics));
        let dir = fixtures_dir(&args).join(name);
        // Freeze the finer candles the zoom actually consulted (empty for the
        // common no-ambiguous-bar case, which writes no `sub_bars.json` at all),
        // so the fixture can reproduce its own verdict instead of degrading to
        // the pessimistic stop offline.
        fixture::save(&dir, &plan, &candles, &meta, &expected, &zoom_bars)?;
        tracing::info!(
            dir = %dir.display(),
            sub_bars = zoom_bars.len(),
            "saved fixture"
        );
    }

    Ok(())
}

/// Build the saved fixture's arming provenance from the `--arm-*` flags plus what
/// this binary knows about itself.
///
/// `engine_version` is stamped from **our own** build (`GIT_VERSION`, a `git
/// describe --tags --dirty`) rather than passed in — it's the version that
/// produced the outcome, so the binary computing it is the only honest source.
/// `tv_arm_version` has to be passed, since that's a different binary.
///
/// An unrecognised `--arm-entry-rule` is preserved verbatim (`EntryRule::Other`)
/// rather than coerced to `normal`: a future strategy flag should show up as
/// itself in the corpus, not silently masquerade as the default.
fn arm_record(args: &Args) -> arm_record::ArmRecord {
    arm_record::ArmRecord {
        entry_rule: arm_record::EntryRule::parse(args.arm_entry_rule.as_deref()),
        skip_calendar_bars: args.arm_skip_calendar_bars,
        skip_golden: args.arm_skip_golden,
        start: args.arm_start.clone(),
        candle_source: Some(args.source.as_str().to_string()),
        chart_symbol: args.arm_chart_symbol.clone(),
        tv_arm_version: args.arm_tv_arm_version.clone(),
        engine_version: Some(env!("GIT_VERSION").to_string()),
        journal_ref: args.trade_ref.clone(),
    }
}

/// Max look-back back-off attempts before we give up widening the warm-up pull.
/// Each attempt grows the span by up to [`MAX_BACKOFF_SPAN_MUL`]× (the density
/// extrapolation, capped), so 6 attempts reach several thousand × the initial
/// wall-clock estimate — enough to hop a weekend gap many times over even on a
/// sparse instrument. Beyond this we replay with whatever warm-up we got and let
/// the report be honest about the shallow ATR rather than loop forever.
const MAX_WARMUP_BACKOFF_ATTEMPTS: u32 = 6;

/// Verdict on one back-off attempt's live-bar count. Widening the warm-up
/// look-back moves `pull_from` EARLIER while `pull_end` stays fixed, so the
/// number of bars at/after `start` — the **live window the engine scores** — must
/// never change. If it does, the series we just pulled is not a superset of the
/// previous one and any result computed from it is not comparable.
///
/// Split out as a pure fn so the invariant is unit-testable without a broker.
#[derive(Debug, PartialEq, Eq)]
enum LiveCountVerdict {
    /// First attempt (nothing to compare against) or an unchanged count.
    Ok,
    /// The live window changed between attempts — a data-source bug, not a
    /// market gap. Carries both counts for the error message.
    Changed { previous: usize, current: usize },
}

/// Compare this attempt's live count against the previous attempt's.
///
/// This is the guard for the candle-cache duplicate-bars bug (fixed in
/// candle-cache v3): overlapping cached/fetched chunks were merged without
/// dedup and then trimmed by COUNT, so a widened look-back could return a
/// DIFFERENT live window — and the replay silently scored it. One unchanged
/// trade plan scored −0.40R or −3.00R depending only on how the look-back
/// landed against the cached-range boundaries.
///
/// Failing loudly here is the point: a silently-different live window produces
/// a plausible-looking R that is simply wrong, which is far worse than an
/// aborted replay ([[no_silent_degrade_prefer_loud_failure]]).
fn check_live_count(previous: Option<usize>, current: usize) -> LiveCountVerdict {
    match previous {
        Some(prev) if prev != current => LiveCountVerdict::Changed {
            previous: prev,
            current,
        },
        _ => LiveCountVerdict::Ok,
    }
}

/// Cap on how much a single warm-up back-off may widen the look-back span,
/// as a multiple of the current span. Guards against a density estimate poisoned
/// by a gap-dominated first attempt (a `--start` on a Monday sees only the
/// weekend's ~0 candles) from leaping back a year-plus and pulling tens of
/// thousands of candles in one shot. A bounded step re-measures against real
/// trading days next round; convergence still fits within
/// [`MAX_WARMUP_BACKOFF_ATTEMPTS`].
const MAX_BACKOFF_SPAN_MUL: i64 = 4;

/// Pull the warm-up prefix + live window, sizing the prefix by a **count of real
/// candles** (`want_warmup`) rather than wall-clock time. Starts from the naive
/// time estimate (`want_warmup × bar` before `start`) and, if a market gap
/// (weekend / session close) leaves fewer than `want_warmup` real candles before
/// `start`, widens the look-back and re-pulls — hopping the gap — until it has
/// enough or exhausts [`MAX_WARMUP_BACKOFF_ATTEMPTS`]. A persistent shortfall
/// WARNs (the ATR may stay cold, e.g. a `needs_golden` enter won't fire) but
/// does not fail: the replay runs on what's available and the trace shows why.
#[allow(clippy::too_many_arguments)]
async fn pull_with_warmup(
    source: CandleSource,
    symbol: &str,
    gran: granularity::ReplayGranularity,
    gran_label: &str,
    start: DateTime<Utc>,
    pull_end: DateTime<Utc>,
    bar_secs: i64,
    want_warmup: usize,
    cache_dir: Option<PathBuf>,
) -> Result<Vec<EngineCandle>> {
    let mut pull_from = start - Duration::seconds(bar_secs * want_warmup as i64);
    let mut attempt: u32 = 0;
    // Live-bar count from the previous attempt, for the invariance guard below.
    let mut prev_live_count: Option<usize> = None;
    loop {
        tracing::info!(
            instrument = %symbol,
            granularity = %gran_label,
            source = ?source,
            warmup_from = %brisbane::bne(pull_from),
            start = %brisbane::bne(start),
            pull_end = %brisbane::bne(pull_end),
            want_warmup,
            attempt,
            "pulling candles (times in Brisbane, UTC+10)"
        );
        let candles =
            candles::pull(source, symbol, gran, pull_from, pull_end, cache_dir.clone()).await?;
        if candles.is_empty() {
            return Err(eyre!(
                "no candles returned for {symbol} {gran_label} in [{pull_from}, {pull_end}]"
            ));
        }
        let warmup_count = candles.iter().filter(|c| c.time < start).count();
        let live_count = candles.len() - warmup_count;
        tracing::info!(
            count = candles.len(),
            warmup = warmup_count,
            live = live_count,
            attempt,
            "pulled candles"
        );

        // The live window must be invariant across back-off attempts: widening
        // only moves `pull_from` earlier, and `pull_end` never moves. A change
        // means the data source handed us a different series, so scoring it
        // would produce a plausible-looking but wrong result. Abort instead.
        if let LiveCountVerdict::Changed { previous, current } =
            check_live_count(prev_live_count, live_count)
        {
            return Err(eyre!(
                "{symbol} {gran_label}: the live-bar count changed between warm-up back-off \
                 attempts ({previous} -> {current}) while only the look-back was widened. The \
                 candle source returned a different series for the same [start, end] window, so \
                 this replay's result would not be comparable. (This was candle-cache's \
                 merge-without-dedup bug, fixed in v3 — if you are seeing it again, the range \
                 fetch is duplicating or dropping bars.)"
            ));
        }
        prev_live_count = Some(live_count);

        if warmup_count >= want_warmup {
            return Ok(candles);
        }
        if attempt >= MAX_WARMUP_BACKOFF_ATTEMPTS {
            tracing::warn!(
                warmup = warmup_count,
                want_warmup,
                "warm-up prefix short of target after {MAX_WARMUP_BACKOFF_ATTEMPTS} back-offs — \
                 likely a market gap (weekend/session close) leaving sparse history. The ATR \
                 may stay cold, so a needs-golden enter can fail to fire. Replaying anyway."
            );
            return Ok(candles);
        }
        // Widen the look-back and retry. Base the next span on the *observed*
        // density (bars per wall-second so far) so we jump past the gap in one
        // step rather than doubling blindly, but never shrink and always at
        // least double, so a zero-density prefix still makes progress.
        let next = next_pull_from(start, pull_from, warmup_count, want_warmup, bar_secs);
        pull_from = next.min(pull_from - Duration::seconds(bar_secs * want_warmup as i64));
        attempt += 1;
    }
}

/// Given the current pull span `[pull_from, start]` yielded `have` real warm-up
/// candles but we want `want`, estimate a new (earlier) `pull_from` that should
/// cover the shortfall. Pure arithmetic so it's unit-testable without a broker.
///
/// Extrapolates from the observed density: the span so far delivered `have`
/// candles over `(start - pull_from)` seconds, so the shortfall `want - have`
/// needs roughly `(want - have) / density` more seconds of look-back. When
/// `have == 0` (the whole span was a gap) density is unknown, so fall back to
/// doubling the current span. The caller additionally clamps so the span never
/// shrinks and always advances by at least the naive estimate.
fn next_pull_from(
    start: DateTime<Utc>,
    pull_from: DateTime<Utc>,
    have: usize,
    want: usize,
    bar_secs: i64,
) -> DateTime<Utc> {
    let span_secs = (start - pull_from).num_seconds().max(bar_secs);
    let shortfall = want.saturating_sub(have);
    let extra_secs = if have == 0 {
        // No density signal — double the current span.
        span_secs
    } else {
        // Seconds-per-real-candle × shortfall, so we reach the target.
        (span_secs as f64 / have as f64 * shortfall as f64).ceil() as i64
    };
    // A safety margin (25%) so a gap that recurs (a second weekend) doesn't leave
    // us one bar short and force another round-trip.
    let extra_secs = extra_secs + extra_secs / 4;
    // Cap the per-attempt jump. When the first attempt lands almost entirely in a
    // market-closed span (a `--start` on a Monday: Sat+Sun return ~0 candles), the
    // density sample is a size-1 (or tiny) outlier — 1 candle over ~2 days — and
    // the extrapolation above would leap back a *year-plus* to find `want` bars,
    // pulling tens of thousands of candles in one shot (AU200 `--start` Mon:
    // 15,612 warmup candles). Clamp each back-off to at most `MAX_BACKOFF_SPAN_MUL`
    // × the current span so a poisoned density estimate can't overshoot: the pull
    // widens a bounded amount, re-measures against real trading days on the next
    // attempt, and converges in 1–2 more rounds (within MAX_WARMUP_BACKOFF_ATTEMPTS)
    // instead of one catastrophic leap.
    let extra_secs = extra_secs.min(span_secs.saturating_mul(MAX_BACKOFF_SPAN_MUL));
    pull_from - Duration::seconds(extra_secs.max(bar_secs))
}

/// The fixtures directory: `--fixtures-dir` if given, else resolved from the
/// **running** process — see [`trade_control_cli::fixtures_dir`].
///
/// Deliberately not `env!("CARGO_MANIFEST_DIR")`: that is the tree this binary
/// was *built* in, which for a deployed CLI is a build directory that may have
/// been deleted (or, worse, still exist and quietly take the write).
fn fixtures_dir(args: &Args) -> PathBuf {
    trade_control_cli::fixtures_dir::resolve(
        args.fixtures_dir.as_deref(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
}

/// Replay saved fixtures offline: one (`--fixture`) or many
/// (`--fixtures-glob`). Dispatches to the batch path when a glob is given.
async fn run_test_mode(args: &Args) -> Result<()> {
    // Chart annotation draws on a live TradingView chart, which test-mode has no
    // connection to — so `--annotate` here is a no-op. Say so rather than accept
    // it silently.
    //
    // Deliberately a warning and not a `conflicts_with`: `tv-arm … replay`
    // *injects* `--annotate true` as its own default, so a hard conflict would
    // reject a chained replay over a flag the operator never typed. Loud, not
    // fatal.
    if args.annotate || args.annotate_unfilled {
        tracing::warn!(
            "--annotate has no effect under --test-mode (there is no live chart to \
             draw on) — ignoring it"
        );
    }
    match args.fixtures_glob.as_deref() {
        Some(pattern) => run_test_mode_batch(args, pattern).await,
        None => run_test_mode_single(args).await,
    }
}

/// Replay one saved fixture. Prints the human report (or a single JSON object
/// under `--json`), and under `--check` diffs the computed outcome against
/// `expected.json`, returning an error (non-zero exit) on any mismatch.
async fn run_test_mode_single(args: &Args) -> Result<()> {
    let name = args.fixture.as_deref().ok_or_else(|| {
        outcome::bad_input(eyre!(
            "--test-mode requires --fixture <name> or --fixtures-glob <glob>"
        ))
    })?;
    let root = fixtures_dir(args);
    let result = replay_one_fixture(args, &root.join(name), name).await;
    if args.json {
        // One object either way — a failure is a row, not silence.
        println!("{}", serde_json::to_string_pretty(&result.row)?);
    }
    // Surface a failure as a non-zero exit, as before. The JSON row above is
    // already printed, so a driver sees BOTH the machine-readable reason and the
    // exit code.
    result.error.map_or(Ok(()), Err)
}

/// Replay every fixture matching `pattern`. A failing fixture is **recorded and
/// the batch continues** — one bad fixture must not hide the other 290.
///
/// Exits non-zero when any fixture failed (or, under `--check`, when any golden
/// mismatched), so a driver can trust the exit code; but the per-fixture rows are
/// always emitted first so it can see exactly which ones and why.
async fn run_test_mode_batch(args: &Args, pattern: &str) -> Result<()> {
    let root = fixtures_dir(args);
    let dirs = batch::matching_fixtures(&root, pattern);
    tracing::info!(
        root = %root.display(),
        pattern,
        matched = dirs.len(),
        "replaying fixture batch offline"
    );

    let mut rows = Vec::with_capacity(dirs.len());
    // Each failing row's typed verdict, captured while its error chain is intact.
    let mut kinds = Vec::new();
    for dir in &dirs {
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unnamed>")
            .to_string();
        let run = replay_one_fixture(args, dir, &name).await;
        if let Some(kind) = run.kind {
            kinds.push(kind);
        }
        rows.push(run.row);
    }
    let summary = batch::BatchSummary::from_results(rows);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!("\n{}", summary.headline());
    }

    if dirs.is_empty() {
        // Not an error — but say so loudly, since an empty batch that reads as
        // success is exactly how a wrong glob produces a silently empty grid.
        //
        // This used to be a `warn!` and `Ok(())`: exit 0, an honest `matched: 0`
        // in the JSON, and a stderr line that vanishes under `RUST_LOG=error`. A
        // driver that checks only the exit code — which is what an exit code is
        // FOR — read a typo'd glob as a clean sweep. Exit 4 instead: the operator
        // named fixtures that don't exist, which is a bad *input*, not an
        // infrastructure blip to retry.
        //
        // The rows are still printed first (an empty `results: []` above), so the
        // JSON contract holds: a document on stdout, plus a failing exit code.
        return Err(outcome::bad_input(eyre!(
            "no fixtures matched {pattern:?} under {} — check the glob and \
             --fixtures-dir. (Nothing ran, so the {:+.2} net R above is vacuous.)",
            root.display(),
            summary.net_r,
        )));
    }
    // Tier-2 scoring. Deliberately BEFORE the `failed > 0` return: a partial
    // sweep is exactly when you most want to see which cells moved, and an early
    // return would throw that report away in favour of a bare exit code. The
    // diff labels itself INCOMPLETE in that case (`unscored`), so it can't be
    // mistaken for a full answer.
    score_against_baseline(args, &summary)?;

    if summary.failed > 0 {
        let msg = eyre!(
            "{} of {} fixture(s) failed — see the rows above (net R {:+.2} excludes them)",
            summary.failed,
            summary.matched,
            summary.net_r,
        );
        // Re-tag with the aggregate of the rows' TYPED verdicts, so the batch exits
        // the same code a single failing fixture would. Emphatically NOT by folding
        // the rows' error text into this message and re-classifying it — the chain
        // is data we don't control (a `--check` mismatch embeds the whole
        // expected+got JSON), so a fixture whose warnings contained a marker phrase
        // could hijack the verdict.
        return Err(match aggregate_kind(&kinds) {
            outcome::FailureKind::CheckMismatch => outcome::check_mismatch(msg),
            outcome::FailureKind::BadInput => outcome::bad_input(msg),
            // Untagged ⇒ infrastructure, the retryable default.
            outcome::FailureKind::Infrastructure => msg,
        });
    }
    Ok(())
}

/// How many movers the human diff lists before truncating.
///
/// The full set is always available as JSON (`--baseline … --json`); this only
/// bounds the *terminal* rendering, and it says how many it withheld.
const DIFF_ROWS_SHOWN: usize = 20;

/// Tier-2 scoring: diff against a blessed baseline, and/or bless this run.
///
/// Both are no-ops unless the operator asked. Neither changes the exit code — a
/// moved number is information, not a failure (that's `--check`'s job).
///
/// Ordering is deliberate: **diff first, then bless.** If both flags are given
/// with the same file, the operator sees what changed before it's overwritten.
/// The reverse order would silently bless a regression and then report a clean
/// diff against the baseline it had just rewritten.
fn score_against_baseline(args: &Args, summary: &batch::BatchSummary) -> Result<()> {
    if let Some(path) = args.baseline.as_deref() {
        let text = fs::read_to_string(path)
            .wrap_err_with(|| format!("reading baseline {}", path.display()))
            .map_err(outcome::bad_input)?;
        let base: baseline::Baseline = serde_json::from_str(&text)
            .wrap_err_with(|| format!("parsing baseline {}", path.display()))
            .map_err(outcome::bad_input)?;

        if !base.is_self_consistent() {
            // Not fatal — the entries are still comparable — but a stored total
            // that disagrees with its own rows means the file was edited by
            // something other than a bless, and the "was" figure below is that
            // stored total.
            tracing::warn!(
                stored = base.net_r,
                recomputed = base.recomputed_net_r(),
                "baseline's stored net R disagrees with its entries — it has been \
                 hand-edited; the 'was' figure below is the stored one"
            );
        }

        let diff = baseline::diff(&base, summary);
        if args.json {
            // A second document on stdout would break the one-object contract
            // `--json` promises, so the diff goes to stderr when JSON is on.
            eprintln!("{}", serde_json::to_string_pretty(&diff)?);
        } else {
            println!("\n{}", baseline::render(&diff, DIFF_ROWS_SHOWN));
        }
    }

    if let Some(path) = args.bless_baseline.as_deref() {
        let blessed = baseline::Baseline::from_summary(summary, args.baseline_label.clone());
        let json = serde_json::to_string_pretty(&blessed)?;
        fs::write(path, format!("{json}\n"))
            .wrap_err_with(|| format!("writing baseline {}", path.display()))?;
        tracing::info!(
            path = %path.display(),
            entries = blessed.entries.len(),
            skipped = summary.failed,
            net_r = blessed.net_r,
            "blessed baseline"
        );
        if summary.failed > 0 {
            // Say it on the way out, not just in a log line: a baseline blessed
            // from a partial sweep is missing rows, and a later diff will report
            // them as `added` rather than as the regressions they might be.
            tracing::warn!(
                skipped = summary.failed,
                "blessed a PARTIAL batch — the failed fixtures are absent from the \
                 baseline and will read as `added` when they come back"
            );
        }
    }
    Ok(())
}

/// The one verdict a batch of mixed failures reports.
///
/// Most-specific wins: a `--check` mismatch is a real regression and must never be
/// masked by a co-occurring corrupt fixture or a flaky mount; bad input beats
/// infrastructure because retrying verbatim cannot fix it. An empty set (or all
/// rows somehow untagged) is infrastructure — the retryable default, since a wrong
/// guess there costs one retry while the other way silently drops a result.
fn aggregate_kind(kinds: &[outcome::FailureKind]) -> outcome::FailureKind {
    if kinds.contains(&outcome::FailureKind::CheckMismatch) {
        return outcome::FailureKind::CheckMismatch;
    }
    if kinds.contains(&outcome::FailureKind::BadInput) {
        return outcome::FailureKind::BadInput;
    }
    outcome::FailureKind::Infrastructure
}

/// One fixture's replay, as a [`batch::BatchResult`] row plus the error (if any)
/// the single-fixture path re-raises.
///
/// Every failure mode — unreadable fixture, missing `expected.json`, a `--check`
/// mismatch — becomes an `ok: false` row rather than an early return, which is
/// what lets a batch keep going and what makes "no row" mean only "the process
/// died unhandled".
struct FixtureRun {
    row: batch::BatchResult,
    error: Option<color_eyre::eyre::Error>,
    /// This row's **typed** verdict, classified at the point of failure while the
    /// real error chain is still intact.
    ///
    /// Load-bearing: a batch has to report one exit code for many failures, and it
    /// must not do that by concatenating the rows' error *text* and re-classifying
    /// the result. The chain contains data we don't control — remote HTTP bodies,
    /// and for a `--check` mismatch the entire pretty-printed expected+got JSON —
    /// so a fixture whose rule ids or warnings happened to contain "no such file"
    /// could flip the whole batch's verdict. Classify each row once, here, then
    /// aggregate the *verdicts*. See `outcome::FailureKind::classify`.
    kind: Option<outcome::FailureKind>,
}

impl FixtureRun {
    fn failed(name: &str, err: color_eyre::eyre::Error) -> Self {
        Self {
            row: batch::BatchResult::failed(name, format!("{err:#}")),
            kind: Some(outcome::FailureKind::classify(&err)),
            error: Some(err),
        }
    }

    /// A fixture that replayed fine but disagreed with its golden.
    ///
    /// Unlike [`Self::failed`] this keeps the row's measurements — the run
    /// produced a real `outcome`, and that number is exactly what you need to
    /// judge whether the regression is benign. See `BatchResult::mismatched`.
    fn mismatched(
        row: batch::BatchResult,
        expected: Option<&economics::ReplayEconomics>,
        err: color_eyre::eyre::Error,
    ) -> Self {
        Self {
            row: batch::BatchResult::mismatched(row, expected, format!("{err:#}")),
            kind: Some(outcome::FailureKind::classify(&err)),
            error: Some(err),
        }
    }
}

async fn replay_one_fixture(args: &Args, dir: &std::path::Path, name: &str) -> FixtureRun {
    tracing::info!(dir = %dir.display(), "replaying fixture offline");
    let inputs = match fixture::load(dir) {
        Ok(i) => i,
        Err(e) => return FixtureRun::failed(name, e),
    };
    let mark_cfg = DetectorMarkConfig::new(
        args.candle_detector_direction,
        args.candle_detector_golden,
        inputs.plan.direction,
    );
    // A fixture whose saved sub-bars don't cover a now-ambiguous bar refetches
    // the missing window from the broker/candle-cache rather than silently
    // scoring it as a stop. The fixture's own `meta` says which source and symbol
    // it came from, so the refetch targets the same feed that produced it.
    let refetch = FixtureRefetch {
        source: inputs.meta.source,
        symbol: &inputs.meta.instrument,
        cache_dir: args.cache_dir.clone(),
    };
    let replay = run_frozen(
        &inputs.plan,
        &inputs.candles,
        inputs.meta.granularity,
        inputs.meta.start,
        mark_cfg,
        &inputs.sub_bars,
        Some(&refetch),
    )
    .await;

    // Market-hours blackout is read from the baked mask keyed on the instrument
    // (`core::intent::market_hours_blocked`) inside `sweep_reason`, so nothing to
    // pass here. Fixtures keep their saved verdict.
    let rendered = report::render(
        &inputs.plan,
        &replay,
        args.simulate,
        args.verbose,
        None,
        &mark_cfg,
    );
    // Under --json the report text would corrupt the JSON on stdout; the rows
    // carry the same numbers, so suppress it.
    if !args.json {
        print!("{}", rendered.text);
    }
    let economics = Some(&rendered.economics);
    let computed = ReplayOutcome::compute(&replay, args.simulate, economics);

    let row = batch::BatchResult::ok(
        name,
        Some(inputs.plan.trade_id.clone()),
        inputs.meta.arm.clone(),
        computed.outcome.clone(),
    );

    if args.check {
        match fixture::load_expected(dir) {
            // A mismatch keeps the row's measurements (`mismatched`, not
            // `failed`): this run scored fine, and its Net R next to the
            // golden's is what makes a red sweep diagnosable rather than just
            // red. The full diff still goes in `error` for a human.
            // Tolerant compare, NOT `!=`: bit-exact float equality made this gate
            // flake across the capture and check paths (see `golden_eq`).
            Ok(expected) if !golden_eq::outcome_matches(&expected, &computed) => {
                let err = diff_error(&expected, &computed);
                return FixtureRun::mismatched(row, expected.outcome.as_ref(), err);
            }
            Ok(_) => tracing::info!(fixture = name, "fixture matches expected.json"),
            Err(e) => return FixtureRun::failed(name, e),
        }
    }

    if args.rebless {
        // `--simulate false` computes no economics, so `computed.outcome` is
        // `None` — and writing that over a golden that HAD economics silently
        // deletes the Net R gate this whole corpus exists to hold. Exit 0, no
        // diff, nothing in the log: the fixture just quietly stops checking the
        // number. Refuse instead. (Recoverable — the next `--check --simulate
        // true` exits 5 — but only if someone happens to run it.)
        if !args.simulate {
            return FixtureRun::failed(
                name,
                outcome::bad_input(eyre!(
                    "refusing to re-bless {name} with --simulate false: the outcome would \
                     carry no economics (net_r / legs), silently removing the Net R gate \
                     from this fixture. Re-bless with simulation on (the default)."
                )),
            );
        }
        if let Err(e) = fixture::save_expected(dir, &computed) {
            return FixtureRun::failed(name, e);
        }
        tracing::info!(dir = %dir.display(), "re-blessed expected.json from frozen inputs");
    }

    FixtureRun {
        row,
        error: None,
        kind: None,
    }
}

/// Run the pure engine over a frozen candle window. Mirrors the live path's
/// `replay::run` call, with a far-future TTL so nothing expires mid-replay (the
/// window's own end isn't needed — the candles are fixed). `live_start` is the
/// saved window start: frozen candles include the warm-up prefix pulled before
/// it, so the plan goes live at `live_start` exactly as it did at save time.
async fn run_frozen(
    plan: &TradePlan,
    candles: &[EngineCandle],
    gran: Granularity,
    live_start: DateTime<Utc>,
    mark_cfg: DetectorMarkConfig,
    // The fixture's saved finer candles (`sub_bars.json`), empty when it has
    // none — either because the saving run had no ambiguous bar, or because the
    // fixture predates `sub_bars.json`.
    saved_sub_bars: &[EngineCandle],
    // Where to re-fetch from when the saved bars don't cover a window the sim
    // asks about. `None` keeps the replay fully offline (the pessimistic stop
    // stands, with a warning).
    refetch: Option<&FixtureRefetch<'_>>,
) -> replay::Replay {
    let expires_at = candles.last().map(|c| c.time).unwrap_or_else(Utc::now) + Duration::days(365);
    // The market-hours gate reads the baked mask keyed on the instrument, so a
    // frozen fixture is gated deterministically with no network. The
    // spread-blackout gate still self-seeds per-bar on `is_ny_close_edge` inside
    // `run` off the frozen candle's own spread.
    //
    // The sub-bar zoom is served from the fixture's OWN saved finer candles, so a
    // fixture whose verdict depended on a zoom reproduces it offline instead of
    // degrading to the pessimistic stop. `FixtureSubBars` also records any window
    // the saved bars don't cover — which happens when the strategy changed since
    // the fixture was saved and a *different* bar is now ambiguous.
    let finer_bar = granularity::finer(gran)
        .map(|f| Duration::seconds(f.engine().seconds()))
        .unwrap_or_else(|| Duration::minutes(1));
    let provider = std::rc::Rc::new(lazy_zoom::FixtureSubBars::new(
        saved_sub_bars.to_vec(),
        finer_bar,
    ));
    let first = replay::run(
        plan,
        candles,
        gran,
        live_start,
        expires_at,
        mark_cfg,
        Some(Box::new(std::rc::Rc::clone(&provider))),
    )
    .await;

    let missed = provider.missed();
    if missed.is_empty() {
        return first;
    }

    // The fixture is missing finer bars for a bar that is ambiguous NOW. Refetch
    // when we're allowed to, so a strategy change doesn't silently score those
    // bars as stops on a fixture that looks complete.
    let Some(refetch) = refetch else {
        tracing::warn!(
            windows = missed.len(),
            first = %missed[0].start,
            "fixture has no saved sub-bars for {} ambiguous window(s) — keeping the \
             pessimistic stop. Re-save the fixture (or allow a refetch) to score \
             these bars properly.",
            missed.len()
        );
        return first;
    };

    let Some(finer_gran) = granularity::finer(gran) else {
        return first;
    };
    let fetched = lazy_zoom::fetch_windows(
        refetch.source,
        refetch.symbol,
        finer_gran,
        &missed,
        refetch.cache_dir.clone(),
    )
    .await;
    if fetched.is_empty() {
        tracing::warn!(
            windows = missed.len(),
            "fixture sub-bar refetch returned nothing — keeping the pessimistic stop"
        );
        return first;
    }

    tracing::info!(
        windows = missed.len(),
        bars = fetched.len(),
        "fixture was missing sub-bars for ambiguous window(s); refetched them"
    );
    // Re-run with the saved bars PLUS the refetched ones, so both the windows the
    // fixture knew about and the newly-ambiguous ones resolve.
    let mut merged = saved_sub_bars.to_vec();
    merged.extend(fetched);
    replay::run(
        plan,
        candles,
        gran,
        live_start,
        expires_at,
        mark_cfg,
        Some(Box::new(lazy_zoom::WindowSubBars::new(merged))),
    )
    .await
}

/// Where a frozen-fixture replay may refetch finer candles from when its saved
/// sub-bars don't cover a window the sim asks about.
///
/// Passing `None` instead keeps a fixture replay fully offline. That matters
/// because `--test-mode` is otherwise a no-broker path (the corpus test runs it
/// under `cargo test` with no credentials), so reaching the network is opt-in
/// per call site rather than assumed.
struct FixtureRefetch<'a> {
    source: CandleSource,
    symbol: &'a str,
    cache_dir: Option<PathBuf>,
}

/// Build a readable diff error when a fixture's computed outcome diverges from
/// its `expected.json` — the two pretty-printed JSON blobs, side by side.
fn diff_error(expected: &ReplayOutcome, got: &ReplayOutcome) -> color_eyre::eyre::Report {
    let exp = serde_json::to_string_pretty(expected).unwrap_or_default();
    let act = serde_json::to_string_pretty(got).unwrap_or_default();
    outcome::check_mismatch(eyre!(
        "fixture outcome does not match expected.json\n--- expected ---\n{exp}\n--- got ---\n{act}"
    ))
}

/// Where one end of the replay window came from.
///
/// Carried so an inverted window can name the two knobs that actually disagree.
/// The window is assembled from up to three independent sources (a flag, the
/// plan, the TV chart), and reporting only the two timestamps left the operator
/// to guess which one to change — see [`WindowSource::describe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowSource {
    /// An explicit `--start` / `--end` flag.
    Flag(&'static str),
    /// The plan's baked `replay_start` (from `tv-arm --start`).
    PlanReplayStart,
    /// The plan's `02-veto-trade-expiry` rule.
    PlanTradeExpiry,
    /// The TradingView chart — the replay cursor (start) or visible-region end.
    Chart(&'static str),
}

impl WindowSource {
    /// A short phrase naming this end's origin *and* how to override it, so the
    /// error reads as an instruction rather than a fact.
    fn describe(self) -> String {
        match self {
            Self::Flag(flag) => format!("the {flag} flag"),
            Self::PlanReplayStart => {
                "the plan's baked replay_start (from `tv-arm --start`); override with --start"
                    .to_string()
            }
            Self::PlanTradeExpiry => {
                "the plan's 02-veto-trade-expiry rule (the expiry drawn on the chart); \
                 override with --end"
                    .to_string()
            }
            Self::Chart(what) => format!("the TradingView chart's {what}; override with --{what}"),
        }
    }
}

/// The fully-resolved replay window: instrument (or `None` to fall back to the
/// plan) and the UTC start/end, each with the source it was resolved from.
/// Granularity is resolved separately (from the plan, see
/// [`resolve_granularity`]).
struct ResolvedWindow {
    instrument: Option<String>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    start_source: WindowSource,
    end_source: WindowSource,
}

/// Explain an inverted (or empty) replay window in terms of the two knobs that
/// disagree, not just the two timestamps.
///
/// The window is assembled from up to three independent sources, so the bare
/// "end must be after start" this replaced named a symptom the operator had no
/// way to map back to a cause: nothing in it said the end came from the plan's
/// trade-expiry while the start came from the chart. The overwhelmingly common
/// shape is exactly that pair — a chart whose expiry line sits *behind* its
/// replay cursor — which `tv-arm --plan-out` arms happily (its lenient
/// build only `warn!`s about a past expiry) and which then surfaces here.
fn inverted_window_error(w: &ResolvedWindow) -> color_eyre::eyre::Report {
    let back = start_end_gap(w);
    let headline = if w.end == w.start {
        format!(
            "the replay window is empty: start and end are the same instant ({})",
            w.start
        )
    } else {
        format!(
            "the replay window runs backwards: it ends {back} before it starts \
             (start {}, end {})",
            w.start, w.end
        )
    };

    // The signature case gets its own closing line, because the fix is on the
    // chart rather than in the flags.
    let hint = if w.start_source == WindowSource::Chart("start")
        && w.end_source == WindowSource::PlanTradeExpiry
    {
        "\n  The trade-expiry drawn on the chart is BEHIND the replay start anchor, so there \
         is nothing to replay. Move the expiry past the start anchor and re-arm, or pass an \
         explicit --end. (`tv-arm --plan-out` only warns about a past expiry, so the plan \
         still armed — this is the replay refusing the resulting window.)"
    } else {
        "\n  Move whichever end is wrong, or override it with the flag named above."
    };

    eyre!(
        "{headline}\n  start = {} — from {}\n  end   = {} — from {}{hint}",
        w.start,
        w.start_source.describe(),
        w.end,
        w.end_source.describe(),
    )
}

/// Render how far the window runs backwards, in whole days/hours/minutes. Keeps
/// the headline readable — "8 days" lands where "-684000s" does not.
fn start_end_gap(w: &ResolvedWindow) -> String {
    let secs = (w.start - w.end).num_seconds().max(0);
    match secs {
        s if s >= 86_400 => format!("{:.1} days", s as f64 / 86_400.0),
        s if s >= 3_600 => format!("{:.1} hours", s as f64 / 3_600.0),
        s if s >= 60 => format!("{} minutes", s / 60),
        s => format!("{s} seconds"),
    }
}

/// Resolve the granularity. Defaults to the plan's granularity; `--granularity`
/// only overrides, and the override must still match the plan (a mismatch would
/// replay the wrong bars through a detector configured for a different bar
/// size, so we refuse it).
fn resolve_granularity(args: &Args, plan: &TradePlan) -> Result<granularity::ReplayGranularity> {
    let plan_label = granularity::engine_label(plan.granularity);
    let Some(raw) = &args.granularity else {
        // No override: take the plan's granularity straight.
        return granularity::parse(plan_label);
    };
    let gran = granularity::parse(raw)?;
    if gran.engine() != plan.granularity {
        return Err(eyre!(
            "granularity {raw} does not match the plan's granularity {plan_label} — \
             drop --granularity to use the plan's, or pass --granularity {plan_label}"
        ));
    }
    Ok(gran)
}

/// Resolve the replay window from flags, the plan, and TradingView. Precedence,
/// per field:
///
///   - **start** — `--start` flag → plan's baked `replay_start` (from
///     `tv-arm --start`) → chart's last shown candle (replay cursor).
///   - **end** — `--end` flag → plan's trade-expiry → chart visible-region end.
///   - **instrument** — `--instrument` flag → chart symbol → (caller) plan.
///
/// TradingView is consulted only when something it provides is actually needed:
/// the start cursor, the symbol, or the end-fallback (and the end-fallback is
/// only reached when the plan has no trade-expiry rule). So a fully-flagged
/// window — or one whose start comes from the plan's `replay_start` and end from
/// its trade-expiry — needs no MCP call. This is what makes a `tv-arm --start`
/// journaling arm self-consistent: the plan carries both ends of the window, so
/// `replay-candles` never has to line up the TV chart's replay cursor.
fn resolve_window(args: &Args, plan: &TradePlan) -> Result<ResolvedWindow> {
    let plan_expiry = trade_expiry_epoch(plan).and_then(|at| Utc.timestamp_opt(at, 0).single());
    let plan_start = plan
        .replay_start
        .and_then(|at| Utc.timestamp_opt(at, 0).single());

    // The chart is needed for the start cursor (only when neither --start nor the
    // plan's replay_start supplies it), the symbol, or (only when the plan has no
    // expiry and no --end) the end fallback.
    let need_start_from_chart = args.start.is_none() && plan_start.is_none();
    let need_end_from_chart = args.end.is_none() && plan_expiry.is_none();
    let need_tv = need_start_from_chart || args.instrument.is_none() || need_end_from_chart;

    let tv = if need_tv {
        let mcp = match &args.tv_mcp_root {
            Some(root) => TvMcp::new(root.clone()),
            None => TvMcp::default(),
        };
        tracing::info!(
            root = %mcp.root().display(),
            "reading replay defaults from TradingView chart"
        );
        Some(tv::pull_defaults(&mcp)?)
    } else {
        None
    };

    let instrument = args
        .instrument
        .clone()
        .or_else(|| tv.as_ref().map(|d: &TvDefaults| d.instrument.clone()));

    let (start, start_source) = match (&args.start, plan_start, &tv) {
        (Some(s), _, _) => (
            parse_start_end(s).wrap_err("parse --start")?,
            WindowSource::Flag("--start"),
        ),
        (None, Some(baked), _) => (baked, WindowSource::PlanReplayStart),
        (None, None, Some(d)) => (d.start, WindowSource::Chart("start")),
        (None, None, None) => {
            unreachable!(
                "need_start_from_chart is true when --start and plan replay_start are both absent"
            )
        }
    };

    let (end, end_source) = match (&args.end, plan_expiry, &tv) {
        (Some(e), _, _) => (
            parse_start_end(e).wrap_err("parse --end")?,
            WindowSource::Flag("--end"),
        ),
        (None, Some(expiry), _) => (expiry, WindowSource::PlanTradeExpiry),
        (None, None, Some(d)) => (d.fallback_end, WindowSource::Chart("end")),
        (None, None, None) => {
            unreachable!("need_end_from_chart is true when --end and plan expiry are both absent")
        }
    };

    Ok(ResolvedWindow {
        instrument,
        start,
        end,
        start_source,
        end_source,
    })
}

/// Pull the plan's trade-expiry as a Unix epoch (seconds, UTC), if it has one.
/// The expiry is a [`Trigger::TimeReached`] rule whose `rule_id` contains
/// `trade-expiry` (e.g. `02-veto-trade-expiry`) — the same id the engine keys
/// on. Returns `None` for a plan with no such rule (the caller then falls back
/// to the chart's visible-region end).
fn trade_expiry_epoch(plan: &TradePlan) -> Option<i64> {
    use trade_control_conventions::AlertBasename;
    plan.rules.iter().find_map(|rule| {
        // Match the trade-expiry *basename* specifically — not the whole
        // `SetupInvalidation` class, which also covers too-high/too-low and the
        // M/W vetos (picking any of those would yield the wrong epoch). The typed
        // parse replaces the old raw `rule_id.contains("trade-expiry")` substring.
        if AlertBasename::parse(&rule.rule_id) != Some(AlertBasename::VetoTradeExpiry) {
            return None;
        }
        match rule.trigger {
            Trigger::TimeReached { at_epoch } => Some(at_epoch),
            _ => None,
        }
    })
}

fn load_plan(path: &PathBuf) -> Result<TradePlan> {
    let text =
        fs::read_to_string(path).wrap_err_with(|| format!("read plan {}", path.display()))?;
    serde_json::from_str(&text).wrap_err_with(|| format!("parse plan JSON {}", path.display()))
}

/// Parse a `--start` / `--end` datetime.
///
/// Delegates to the **shared** parser in the cli library so `tv-arm --start`
/// and `replay-candles --start` cannot disagree — `tv-arm ... replay` forwards
/// the flag verbatim, so two parsers meant the arm cursor and the replay window
/// could point at different instants. Tagged as bad *input* here (the library
/// returns a plain error, since it has no exit-code vocabulary).
fn parse_start_end(s: &str) -> Result<DateTime<Utc>> {
    trade_control_cli::start_time::parse_start_end(s).map_err(outcome::bad_input)
}

/// Emit the clap-generated zsh completion script. Binds the completion to the
/// invoked binary name (argv[0] stem) so a renamed-on-install copy emits
/// completions for its own name, falling back to the clap command name. Mirrors
/// the `tv-arm --print-completions` pattern.
fn print_completions() {
    let mut cmd = Args::command();
    let name = std::env::args()
        .next()
        .and_then(|a| {
            std::path::Path::new(&a)
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| cmd.get_name().to_string());
    generate(Shell::Zsh, &mut cmd, name, &mut std::io::stdout());
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(ErrorLayer::default())
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_cli::replay_args::{DirectionFilter, GoldenFilter};
    use trade_control_engine::Granularity;

    #[test]
    fn bare_datetime_is_brisbane_minute_and_second_precision() {
        // A bare (offset-less) datetime is Brisbane (+10). 17:00 Brisbane is
        // 07:00 UTC. Minute and second precision agree.
        let a = parse_start_end("2026-06-30T17:00").unwrap();
        let b = parse_start_end("2026-06-30T17:00:00").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, Utc.with_ymd_and_hms(2026, 6, 30, 7, 0, 0).unwrap());
    }

    #[test]
    fn explicit_offset_is_honoured() {
        // `+10:00` spelled out == the bare Brisbane reading.
        assert_eq!(
            parse_start_end("2026-06-30T17:00+10:00").unwrap(),
            parse_start_end("2026-06-30T17:00").unwrap(),
        );
        // `Z` is UTC, not Brisbane.
        assert_eq!(
            parse_start_end("2026-06-30T07:00Z").unwrap(),
            Utc.with_ymd_and_hms(2026, 6, 30, 7, 0, 0).unwrap(),
        );
        // An arbitrary offset is respected: 09:00+02:00 == 07:00 UTC.
        assert_eq!(
            parse_start_end("2026-06-30T09:00:00+02:00").unwrap(),
            Utc.with_ymd_and_hms(2026, 6, 30, 7, 0, 0).unwrap(),
        );
    }

    #[test]
    fn rejects_garbage_datetime() {
        assert!(parse_start_end("yesterday").is_err());
    }

    fn window(
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        start_source: WindowSource,
        end_source: WindowSource,
    ) -> ResolvedWindow {
        ResolvedWindow {
            instrument: None,
            start,
            end,
            start_source,
            end_source,
        }
    }

    /// The real GBP/JPY failure (2026-08-06): chart replay cursor at 07-31
    /// 14:00, plan trade-expiry at 07-23 17:00 — the window runs 8 days
    /// backwards.
    ///
    /// The message this replaced was `end (…) must be after start (…)`: two
    /// timestamps and nothing else, so the operator could not tell that the end
    /// came from a rule baked into the plan while the start came from the chart.
    /// Every assertion below is a thing that message could NOT answer.
    #[test]
    fn inverted_window_names_both_sources_and_the_fix() {
        let start = Utc.with_ymd_and_hms(2026, 7, 31, 14, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 7, 23, 17, 0, 0).unwrap();
        let msg = inverted_window_error(&window(
            start,
            end,
            WindowSource::Chart("start"),
            WindowSource::PlanTradeExpiry,
        ))
        .to_string();

        // Says WHICH WAY it is wrong, and by how much, in human units.
        assert!(msg.contains("runs backwards"), "got: {msg}");
        assert!(msg.contains("7.9 days"), "got: {msg}");
        // Names the origin of each end — the whole point of the change.
        assert!(msg.contains("TradingView chart"), "got: {msg}");
        assert!(msg.contains("02-veto-trade-expiry"), "got: {msg}");
        // Tells the operator where the fix lives (the chart, not a flag) and
        // explains why arming still succeeded.
        assert!(
            msg.contains("Move the expiry past the start anchor"),
            "got: {msg}"
        );
        assert!(msg.contains("only warns about a past expiry"), "got: {msg}");
        // Both instants still present — the old message's only content.
        assert!(msg.contains("2026-07-31 14:00:00 UTC"), "got: {msg}");
        assert!(msg.contains("2026-07-23 17:00:00 UTC"), "got: {msg}");
    }

    /// An equal start/end is empty, not backwards — "ends 0 seconds before it
    /// starts" would be nonsense.
    #[test]
    fn an_empty_window_is_described_as_empty_not_backwards() {
        let t = Utc.with_ymd_and_hms(2026, 7, 31, 14, 0, 0).unwrap();
        let msg = inverted_window_error(&window(
            t,
            t,
            WindowSource::Flag("--start"),
            WindowSource::Flag("--end"),
        ))
        .to_string();
        assert!(msg.contains("empty"), "got: {msg}");
        assert!(!msg.contains("runs backwards"), "got: {msg}");
    }

    /// When the operator passed the flags explicitly, the chart-specific advice
    /// would be actively misleading — there is no expiry line to move.
    #[test]
    fn flag_sourced_window_does_not_blame_the_chart() {
        let start = Utc.with_ymd_and_hms(2026, 7, 31, 14, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 7, 31, 12, 0, 0).unwrap();
        let msg = inverted_window_error(&window(
            start,
            end,
            WindowSource::Flag("--start"),
            WindowSource::Flag("--end"),
        ))
        .to_string();
        assert!(msg.contains("the --start flag"), "got: {msg}");
        assert!(msg.contains("the --end flag"), "got: {msg}");
        assert!(!msg.contains("trade-expiry"), "got: {msg}");
        // Sub-day gaps read in hours, not "0.1 days".
        assert!(msg.contains("2.0 hours"), "got: {msg}");
    }

    /// The window error must stay `bad-input` (exit 4, "fix your input"), never
    /// infrastructure (exit 3, "retry me") — retrying an inverted window
    /// forever is exactly the silent cell-loss `outcome` exists to prevent.
    #[test]
    fn inverted_window_is_classified_bad_input() {
        let start = Utc.with_ymd_and_hms(2026, 7, 31, 14, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 7, 23, 17, 0, 0).unwrap();
        let err = outcome::bad_input(inverted_window_error(&window(
            start,
            end,
            WindowSource::Chart("start"),
            WindowSource::PlanTradeExpiry,
        )));
        assert_eq!(
            outcome::FailureKind::classify(&err),
            outcome::FailureKind::BadInput,
        );
    }

    /// Build a minimal `TradePlan` from JSON, with the given rules spliced in.
    /// Plans are loaded from JSON in the real flow, so exercising serde here
    /// also confirms the rule shapes the resolver reads match the wire form.
    fn plan_with_rules(granularity: &str, rules_json: &str) -> TradePlan {
        let json = format!(
            r#"{{
                "trade_id": "test-1",
                "instrument": "EUR_USD",
                "direction": "long",
                "granularity": "{granularity}",
                "pip_size": 0.0001,
                "rules": {rules_json}
            }}"#
        );
        serde_json::from_str(&json).expect("parse test plan")
    }

    /// A single rule JSON with the given id + a `TimeReached` trigger. The
    /// intent is the minimal set of non-defaulted `Intent` fields.
    fn time_rule(rule_id: &str, at_epoch: i64) -> String {
        format!(
            r#"{{
                "rule_id": "{rule_id}",
                "trigger": {{ "type": "time_reached", "at_epoch": {at_epoch} }},
                "fire_mode": "once",
                "intent": {{
                    "v": 1,
                    "id": "{rule_id}-intent",
                    "not_after": "2027-01-01T00:00:00Z",
                    "action": "veto",
                    "instrument": "EUR_USD"
                }}
            }}"#
        )
    }

    #[test]
    fn extracts_trade_expiry_epoch() {
        let expiry = Utc
            .with_ymd_and_hms(2026, 6, 16, 15, 0, 0)
            .unwrap()
            .timestamp();
        let rules = format!("[{}]", time_rule("02-veto-trade-expiry", expiry));
        let plan = plan_with_rules("h1", &rules);
        assert_eq!(trade_expiry_epoch(&plan), Some(expiry));
    }

    #[test]
    fn ignores_non_expiry_time_rules() {
        // A plan with a time rule that isn't the trade-expiry (a pause window)
        // has no recoverable expiry.
        let rules = format!("[{}]", time_rule("pause-start-news1", 1_780_000_000));
        let plan = plan_with_rules("h1", &rules);
        assert_eq!(trade_expiry_epoch(&plan), None);
    }

    #[test]
    fn no_rules_means_no_expiry() {
        let plan = plan_with_rules("h1", "[]");
        assert_eq!(trade_expiry_epoch(&plan), None);
    }

    #[test]
    fn granularity_defaults_to_plan() {
        let plan = plan_with_rules("h1", "[]");
        let args = base_args();
        let gran = resolve_granularity(&args, &plan).unwrap();
        assert_eq!(gran.engine(), Granularity::H1);
    }

    #[test]
    fn granularity_override_matching_plan_is_accepted() {
        let plan = plan_with_rules("h1", "[]");
        let mut args = base_args();
        args.granularity = Some("1h".into());
        assert_eq!(
            resolve_granularity(&args, &plan).unwrap().engine(),
            Granularity::H1
        );
    }

    #[test]
    fn granularity_override_mismatching_plan_is_rejected() {
        let plan = plan_with_rules("h1", "[]");
        let mut args = base_args();
        args.granularity = Some("5m".into());
        let err = resolve_granularity(&args, &plan).unwrap_err().to_string();
        assert!(err.contains("does not match"), "got: {err}");
    }

    /// The plan's baked `replay_start` (from `tv-arm --start`) is used as the
    /// window start when `--start` isn't passed — no TV chart cursor needed.
    /// (instrument + end are flagged so `resolve_window` makes no MCP call.)
    #[test]
    fn window_start_comes_from_plan_replay_start() {
        let mut plan = plan_with_rules("h1", "[]");
        plan.replay_start = Some(1_781_208_000); // 2026-06-11 20:00 UTC
        let mut args = base_args();
        args.instrument = Some("EUR_USD".into());
        args.end = Some("2026-06-21T22:00:00Z".into());
        let w = resolve_window(&args, &plan).expect("resolve");
        assert_eq!(w.start, Utc.timestamp_opt(1_781_208_000, 0).unwrap());
        assert_eq!(w.end, Utc.with_ymd_and_hms(2026, 6, 21, 22, 0, 0).unwrap());
    }

    /// An explicit `--start` flag overrides the plan's baked `replay_start`.
    #[test]
    fn window_start_flag_overrides_plan_replay_start() {
        let mut plan = plan_with_rules("h1", "[]");
        plan.replay_start = Some(1_781_208_000);
        let mut args = base_args();
        args.instrument = Some("EUR_USD".into());
        args.end = Some("2026-06-21T22:00:00Z".into());
        args.start = Some("2026-06-12T00:00:00Z".into());
        let w = resolve_window(&args, &plan).expect("resolve");
        assert_eq!(w.start, Utc.with_ymd_and_hms(2026, 6, 12, 0, 0, 0).unwrap());
    }

    /// `Args` with only `--plan` set; the rest at their defaults. Lets the
    /// resolver tests flip individual flags.
    fn base_args() -> Args {
        Args {
            plan: Some(PathBuf::from("unused.json")),
            instrument: None,
            granularity: None,
            source: CandleSource::TradeNation,
            start: None,
            end: None,
            tv_mcp_root: None,
            simulate: true,
            verbose: false,
            candle_detector_direction: DirectionFilter::With,
            candle_detector_golden: GoldenFilter::Golden,
            annotate: false,
            annotate_unfilled: false,
            arm_entry_rule: None,
            arm_skip_calendar_bars: false,
            arm_skip_golden: false,
            arm_start: None,
            arm_chart_symbol: None,
            arm_tv_arm_version: None,
            trade_ref: None,
            fixtures_glob: None,
            baseline: None,
            bless_baseline: None,
            baseline_label: None,
            json: false,
            warmup_bars: 200,
            cache_dir: None,
            print_completions: false,
            save: None,
            message: None,
            test_mode: false,
            fixture: None,
            check: false,
            rebless: false,
            fixtures_dir: None,
        }
    }

    // ---- warm-up back-off (`next_pull_from`) --------------------------------

    const M15: i64 = 15 * 60;

    /// A dense span (no gap) extrapolates linearly: got half the target over the
    /// span, so the next look-back roughly doubles it (plus the 25% margin) to
    /// reach the whole target.
    #[test]
    fn next_pull_from_extrapolates_from_density() {
        let start = Utc.with_ymd_and_hms(2026, 7, 6, 1, 30, 0).unwrap();
        // 48 candles over a 48-bar span → density 1 candle/bar. Want 96 → need
        // 48 more bars of look-back.
        let pull_from = start - Duration::seconds(M15 * 48);
        let next = next_pull_from(start, pull_from, 48, 96, M15);
        // Extra = 48 bars × (1 + 0.25 margin) = 60 bars earlier than pull_from.
        let extra = (pull_from - next).num_seconds();
        assert_eq!(extra, M15 * 60, "shortfall × density + 25% margin");
    }

    /// The pathological weekend-gap case: the whole span landed in a gap and
    /// returned zero real candles. With no density signal, fall back to doubling
    /// the span so we still make progress toward hopping the gap.
    #[test]
    fn next_pull_from_doubles_when_span_was_all_gap() {
        let start = Utc.with_ymd_and_hms(2026, 7, 6, 1, 30, 0).unwrap();
        let pull_from = start - Duration::seconds(M15 * 200); // 50h wall-time
        let next = next_pull_from(start, pull_from, 0, 96, M15);
        // have == 0 → extra = span (200 bars) + 25% = 250 bars earlier.
        let extra = (pull_from - next).num_seconds();
        assert_eq!(
            extra,
            M15 * 250,
            "zero-density falls back to doubling +margin"
        );
        assert!(next < pull_from, "always reaches further back");
    }

    /// The poisoned-density case that pulled 15k candles: a `--start` on a Monday
    /// makes the first attempt's span land almost entirely in the weekend, so it
    /// returns just 1 real candle. The raw extrapolation (1 candle / 200-bar span,
    /// shortfall 95) would leap back ~19,000 bars (~200 days); the cap clamps the
    /// jump to MAX_BACKOFF_SPAN_MUL × the current span so it never overshoots.
    #[test]
    fn next_pull_from_caps_a_gap_poisoned_density_jump() {
        let start = Utc.with_ymd_and_hms(2026, 7, 20, 0, 0, 0).unwrap();
        let pull_from = start - Duration::seconds(M15 * 200); // naive 200-bar span
        // have = 1 (weekend returned a single candle), want 96 → shortfall 95.
        let next = next_pull_from(start, pull_from, 1, 96, M15);
        let extra = (pull_from - next).num_seconds();
        let span = (start - pull_from).num_seconds();
        assert_eq!(
            extra,
            span * MAX_BACKOFF_SPAN_MUL,
            "a size-1 density sample is capped at {MAX_BACKOFF_SPAN_MUL}× the span, \
             not extrapolated to ~200 days"
        );
        // Sanity: the uncapped extrapolation really would have been enormous.
        let uncapped = (span as f64 / 1.0 * 95.0) as i64;
        assert!(
            uncapped > span * MAX_BACKOFF_SPAN_MUL * 10,
            "the cap materially bounds the jump (uncapped {uncapped}s ≫ cap)"
        );
    }

    /// A healthy density extrapolation below the cap is left untouched — the cap
    /// only clamps outliers, it doesn't shorten normal back-offs.
    #[test]
    fn next_pull_from_cap_does_not_shrink_a_healthy_jump() {
        let start = Utc.with_ymd_and_hms(2026, 7, 6, 1, 30, 0).unwrap();
        // 48/96 over a 48-bar span → extra = 60 bars (< 4× the 48-bar span = 192).
        let pull_from = start - Duration::seconds(M15 * 48);
        let next = next_pull_from(start, pull_from, 48, 96, M15);
        let extra = (pull_from - next).num_seconds();
        assert_eq!(
            extra,
            M15 * 60,
            "healthy extrapolation unchanged by the cap"
        );
    }

    /// The first attempt has nothing to compare against.
    #[test]
    fn live_count_guard_passes_on_the_first_attempt() {
        assert_eq!(check_live_count(None, 153), LiveCountVerdict::Ok);
    }

    /// The normal case: widening the look-back adds warm-up bars and leaves the
    /// live window untouched.
    #[test]
    fn live_count_guard_passes_when_the_live_window_is_stable() {
        assert_eq!(check_live_count(Some(153), 153), LiveCountVerdict::Ok);
    }

    /// The regression this guard exists for (candle-cache merge-without-dedup,
    /// fixed in v3): a widened look-back came back with FEWER live bars, because
    /// duplicates from an overlapping chunk merge displaced real bars off the
    /// count-based trim. The replay then scored a different window — one
    /// unchanged Coffee M15 plan reported −0.40R or −3.00R depending only on the
    /// cursor. Silently scoring that is the failure mode; abort instead.
    #[test]
    fn live_count_guard_catches_a_shrinking_live_window() {
        assert_eq!(
            check_live_count(Some(153), 135),
            LiveCountVerdict::Changed {
                previous: 153,
                current: 135
            },
            "the exact 153 -> 135 drop observed on Coffee M15"
        );
    }

    /// Growth is equally wrong — the live window is bounded by `pull_end`, which
    /// never moves, so gaining bars means the source changed its answer too.
    #[test]
    fn live_count_guard_catches_a_growing_live_window() {
        assert_eq!(
            check_live_count(Some(135), 153),
            LiveCountVerdict::Changed {
                previous: 135,
                current: 153
            },
            "a GROWING live window is just as much a source inconsistency"
        );
    }

    /// Never returns a `pull_from` at or after the existing one (monotonic
    /// progress), even for a degenerate near-target span.
    #[test]
    fn next_pull_from_always_moves_earlier() {
        let start = Utc.with_ymd_and_hms(2026, 7, 6, 1, 30, 0).unwrap();
        let pull_from = start - Duration::seconds(M15 * 96);
        let next = next_pull_from(start, pull_from, 95, 96, M15);
        assert!(
            next <= pull_from - Duration::seconds(M15),
            "advances ≥ 1 bar"
        );
    }
}
