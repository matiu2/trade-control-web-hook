//! Loading a plan into the live TradingView chart via the Node-side
//! `tradingview-mcp-jackson` CLI. "Load" = set the chart's symbol and timeframe
//! **only** — the operator scrolls/zooms to the setup manually. It does **not**
//! scroll to the anchor, set a visible range, or draw anything — the replay
//! `--annotate` path (on the Replay screen) owns drawing.
//!
//! Each step shells `node <root>/src/cli/index.js <cmd> …`. TradingView needs a
//! beat to catch up between commands, so we sleep ~1s between the symbol and
//! timeframe commands (calibrated interactively — without it the symbol change
//! races the timeframe change).
//!
//! ## Already-there short-circuit
//!
//! [`load_chart`] first **reads** the chart (`tv state`) and returns early when
//! the symbol and resolution already match. Measured against the live chart
//! (2026-08-04): an already-there hit costs **~75-150ms**, where a real load is
//! **~13s** — the two node spawns and TradingView's own settling dominate, far
//! more than the 1s pause below suggests. So the hit is ~175× cheaper.
//!
//! Speed isn't the main point though: setting the symbol **resets the operator's
//! scroll position**, so re-loading a chart that was already correct throws away
//! the exact thing they navigated to. Reading first makes `l` / replay /
//! fixture-capture idempotent.
//!
//! The comparison is safe because `tv state` reports the symbol
//! fully-qualified (`TRADENATION:EURCAD`) and the resolution in TradingView's own
//! form (`240`) — the *same* forms [`tv_symbol`] and [`tv_resolution`] produce.
//! So it is a plain string equality against values we already compute, with no
//! second mapping to drift. Anything unexpected (`success: false`, a missing
//! field, unparseable JSON, a spawn failure) **falls through to a normal load**:
//! a redundant load costs a second, while a wrong skip leaves the operator on
//! the wrong chart and — since tv-arm re-arms from whatever chart is up — yields
//! a *wrong answer*, not a slow one.
//!
//! ## `--new-tv`: a second chart backend
//!
//! This module also hosts [`ChartBackend`] and [`load_chart_backend`], the
//! dispatch `--new-tv` uses to point `l` at `local-chart` (`local_chart`
//! submodule) instead of this file's TradingView path. The TradingView
//! functions below (`load_chart` and everything it calls) are untouched by
//! that addition — `--new-tv` absent is byte-identical to before it existed.

mod local_chart;

pub use local_chart::{DEFAULT_LOCAL_CHART_URL, arm_setup_url, load_chart_local};

use std::path::PathBuf;
use std::process::Command;
use std::thread::sleep;
use std::time::Duration;

use color_eyre::eyre::{Result, eyre};
use serde::Deserialize;

/// The Node tv-mcp checkout. Matches `trading_view::mcp::DEFAULT_TV_MCP_ROOT`
/// and the hard-coded path in `replay-candles`. One-user tool, fine hard-coded.
const TV_MCP_ROOT: &str = "/home/matiu/Downloads/tradingview-mcp-jackson";

/// Pause between tv-mcp commands so TradingView can catch up.
const STEP_PAUSE: Duration = Duration::from_millis(1000);

/// Which chart the `l` key drives. Default is TradingView (today's behaviour,
/// unchanged); `--new-tv` at startup switches every plan to `LocalChart`.
///
/// An enum rather than a bare `bool`: adding a third backend later is then a
/// compile error at every match site instead of a silently-wrong default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChartBackend {
    /// The existing tv-mcp-driven path — [`load_chart`], byte-identical.
    TradingView,
    /// `--new-tv [URL]` — local-chart, at `base_url` (no trailing slash).
    LocalChart { base_url: String },
}

impl ChartBackend {
    /// A short label for the UI indicator (footer/header), so the active
    /// backend is never a silent surprise when `l` is pressed.
    pub fn label(&self) -> &str {
        match self {
            ChartBackend::TradingView => "TradingView",
            ChartBackend::LocalChart { .. } => "local-chart",
        }
    }

    /// The local-chart base URL when that backend is active, else `None`.
    ///
    /// Used by the RAW replay (`R`), which drives `replay-candles` directly on
    /// the stored plan and so has no tv-arm to route through — it needs the URL
    /// itself to draw the positions file `replay-candles` writes. `None` keeps
    /// that path on TradingView's `--annotate`, byte-identical to before.
    pub fn local_chart_url(&self) -> Option<&str> {
        match self {
            ChartBackend::TradingView => None,
            ChartBackend::LocalChart { base_url } => Some(base_url),
        }
    }

    /// The `--spec-url` tv-arm should arm from for a plan on this backend, or
    /// `None` when it must read the live TradingView chart instead.
    ///
    /// **This is what makes `--new-tv` a whole backend rather than half of
    /// one.** `l` (load-chart) and `r`/`s` (replay / fixture-capture) are two
    /// independent chart seams: the load job dispatches on the backend, but
    /// tv-arm defaults to reading TradingView via tv-mcp. So before this
    /// existed, `--new-tv` loaded local-chart and then armed off whatever
    /// symbol and leftover drawings the TradingView tab was sitting on —
    /// silently, since a stale chart still arms *something*. The observed
    /// failure was an `ihs-eur-cad` replay rejected for an invalidation line at
    /// 1.61982 when the plan's own `too-low` was 1.60942: a different setup's
    /// drawing entirely, caught only because `tv-arm/src/hs_resolve.rs`'s
    /// fib-range guard happened to reject it. A stale line that fell *inside*
    /// the range would have replayed a wrong setup with no error at all.
    ///
    /// `None` for TradingView is not a gap — tv-arm reading the live chart IS
    /// that backend's arm path, and passing no `--spec-url` is exactly today's
    /// behaviour, byte-identical.
    pub fn spec_url(&self, instrument: &str, broker: &str, granularity: &str) -> Option<String> {
        match self {
            ChartBackend::TradingView => None,
            ChartBackend::LocalChart { base_url } => {
                arm_setup_url(base_url, instrument, broker, granularity)
            }
        }
    }
}

/// Load a plan's chart on whichever backend is active. Same return contract as
/// [`load_chart`] (`Ok(true)` = already there, nothing changed), dispatching to
/// the TradingView path unchanged or the new local-chart path.
///
/// local-chart has no server-readable "what's on screen right now" (see
/// `local_chart`'s module docs) — so its `Ok(true)` never fires; every call
/// opens/navigates. That is the SAME fail-open direction the TradingView path
/// documents ("answers false on any doubt"): the failure mode of always
/// reloading is a wasted browser navigation, never a stranded operator.
pub fn load_chart_backend(
    backend: &ChartBackend,
    instrument: &str,
    broker: &str,
    granularity: &str,
    goto: Option<&str>,
) -> Result<bool> {
    match backend {
        // TradingView sets symbol + timeframe only; scrolling to a date is a
        // tv-mcp capability this path has never had (and the operator asked
        // for the jump on local-chart precisely because it was too hard
        // there). Ignored rather than faked.
        ChartBackend::TradingView => load_chart(instrument, broker, granularity),
        ChartBackend::LocalChart { base_url } => {
            load_chart_local(base_url, instrument, broker, granularity, goto)
        }
    }
}

/// Load a plan onto the live chart: set the symbol and timeframe **only**. The
/// operator scrolls/zooms to the setup manually — we deliberately do **not**
/// scroll to the anchor or set a visible range. `instrument` is the plan's raw
/// id (OANDA/TradeNation form), `broker` its broker (`oanda`/`tradenation`) —
/// which fixes the TradingView *exchange prefix* so the right broker's chart
/// loads — `granularity` its `h1`/`m15`/… string.
///
/// Returns `Ok(true)` when the chart was **already** on this symbol+timeframe and
/// nothing was changed, `Ok(false)` when it was actually loaded.
pub fn load_chart(instrument: &str, broker: &str, granularity: &str) -> Result<bool> {
    let symbol = tv_symbol(instrument, broker)?;
    let resolution = tv_resolution(granularity)?;

    // Read before writing: a chart already on this setup needs no load, and
    // re-setting the symbol would reset the operator's scroll position. Any
    // doubt about the current state falls through to the load below.
    if chart_matches(&symbol, &resolution) {
        return Ok(true);
    }

    // 1. symbol, 2. timeframe — the symbol change needs a beat before the
    //    timeframe change or the two race.
    tv(&["symbol", &symbol])?;
    sleep(STEP_PAUSE);
    tv(&["timeframe", &resolution])?;
    Ok(false)
}

/// The subset of `tv state` we read. `symbol` is fully-qualified
/// (`TRADENATION:EURCAD`) and `resolution` is TradingView's own form (`240`) —
/// both already what we compute, so no re-mapping.
///
/// Extra fields (`chartType`, `studies`, …) are ignored rather than
/// `deny_unknown_fields`: this is a *fast path*, and a new field appearing
/// upstream should not turn every load into a hard error.
#[derive(Debug, Deserialize)]
struct ChartState {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    resolution: Option<String>,
}

/// Is the live chart already showing `symbol` at `resolution`?
///
/// **Answers `false` on any doubt** — a failed spawn, a non-zero exit,
/// unparseable JSON, `success: false`, or a missing field. The caller then does a
/// normal load. This asymmetry is deliberate: a needless load costs ~13s, while
/// a wrong skip strands the operator on the wrong chart, and tv-arm re-arms from
/// whatever chart is up — so the failure mode is a wrong answer, not a slow one.
/// Slow is recoverable by waiting; wrong is not recoverable at all.
fn chart_matches(symbol: &str, resolution: &str) -> bool {
    read_state().is_some_and(|state| state_matches(&state, symbol, resolution))
}

/// The pure half of [`chart_matches`]: does this already-read state match?
/// Split out so the decision is unit-testable without a live chart.
fn state_matches(state: &ChartState, symbol: &str, resolution: &str) -> bool {
    if !state.success {
        return false;
    }
    let (Some(current_symbol), Some(current_resolution)) = (&state.symbol, &state.resolution)
    else {
        return false;
    };
    // Symbols are case-insensitive on TradingView (`OANDA:EURUSD`); resolutions
    // are exact tokens (`60`, `1D`) so compare them case-insensitively too rather
    // than assuming a casing for `1d`/`1D`.
    current_symbol.eq_ignore_ascii_case(symbol)
        && current_resolution.eq_ignore_ascii_case(resolution)
}

/// Read the live chart state, or `None` if it can't be read/parsed for any
/// reason. Never returns an error — this is a best-effort fast path, and every
/// failure is handled identically by falling back to a load.
fn read_state() -> Option<ChartState> {
    let out = Command::new("node")
        .arg(cli_path())
        .arg("state")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Resolve a plan instrument id to a **fully-qualified** TradingView symbol
/// (`EXCHANGE:SYMBOL`, e.g. `TRADENATION:AUDCHF`). instrument-lookup returns the
/// bare symbol (`AUDCHF`); without the exchange prefix TradingView picks its own
/// default exchange (OANDA for FX), which loaded the *wrong broker's* chart for
/// a TradeNation plan. So we prepend the exchange for the plan's actual broker.
fn tv_symbol(instrument: &str, broker: &str) -> Result<String> {
    use instrument_lookup::{Broker, by_broker_symbol};
    // Resolve the bare TV symbol, trying both broker views of the raw id.
    // Some catalog entries (many FX crosses, e.g. AUD/SGD) carry an OANDA/TN
    // symbol but a blank `tradingview` field, so `symbol_for(TradingView)`
    // returns None. TradingView symbols are just the raw id with separators
    // stripped, so fall back to that rather than failing: `AUD_SGD` → `AUDSGD`,
    // `AUD/SGD` → `AUDSGD`. Keeps any unknown FX instrument loadable.
    let bare = [Broker::Oanda, Broker::TradeNation]
        .into_iter()
        .find_map(|b| {
            by_broker_symbol(b, instrument)
                .ok()
                .flatten()
                .and_then(|asset| asset.symbol_for(Broker::TradingView))
                .map(str::to_string)
        })
        .unwrap_or_else(|| strip_separators(instrument));

    match tv_exchange(broker) {
        Some(exchange) => Ok(format!("{exchange}:{bare}")),
        // Unknown/blank broker: fall back to the bare symbol (TV's default
        // exchange), preserving prior behaviour rather than failing.
        None => Ok(bare),
    }
}

/// Turn a raw broker instrument id into a bare symbol by dropping the
/// separators brokers use but TradingView/local-chart don't: `AUD_SGD`
/// (OANDA form) and `AUD/SGD` (TradeNation form) both → `AUDSGD`. Used as
/// the last-resort fallback when instrument-lookup has no symbol for the
/// asset on the target — TradingView here, OANDA in `local_chart`. Plain
/// private (no `pub`): a private item in a parent module is already visible
/// to its own submodules in Rust, so `local_chart` reuses this directly via
/// `super::strip_separators` rather than duplicating the same filter.
fn strip_separators(instrument: &str) -> String {
    instrument
        .chars()
        .filter(|c| !matches!(c, '_' | '/' | ' '))
        .collect()
}

/// The TradingView exchange prefix for a plan broker. `None` for an
/// unknown/blank broker (caller falls back to a bare symbol).
/// `ibkr` is deliberately absent. A futures chart's TradingView exchange is a
/// property of the *contract*, not the broker — gold is `COMEX`, the equity
/// index futures are `CME` — so there is no single prefix this signature could
/// return. An IBKR plan therefore falls through to the bare symbol and opens
/// TradingView's default-exchange chart, which is the same graceful degradation
/// an unknown broker has always had. Fixing it properly needs the contract root,
/// which arrives with the catalog work in Stage 5 of the IBKR integration.
fn tv_exchange(broker: &str) -> Option<&'static str> {
    match broker.to_ascii_lowercase().as_str() {
        "tradenation" => Some("TRADENATION"),
        "oanda" => Some("OANDA"),
        _ => None,
    }
}

/// Map a plan granularity (`m1`/`m15`/`h1`/`h4`/`d`) to a TradingView
/// resolution string (`1`/`15`/`60`/`240`/`1D`).
fn tv_resolution(granularity: &str) -> Result<String> {
    let g = granularity.to_ascii_lowercase();
    let res = match g.as_str() {
        "m1" => "1",
        "m5" => "5",
        "m15" => "15",
        "m30" => "30",
        "h1" => "60",
        "h4" => "240",
        "d" | "d1" | "1d" => "1D",
        "w" | "w1" | "1w" => "1W",
        other => return Err(eyre!("unknown granularity `{other}`")),
    };
    Ok(res.to_string())
}

/// The tv-mcp CLI entrypoint.
fn cli_path() -> PathBuf {
    PathBuf::from(TV_MCP_ROOT).join("src/cli/index.js")
}

/// Shell one tv-mcp command (`node <cli> <args>`), surfacing stderr on failure.
fn tv(args: &[&str]) -> Result<()> {
    let cli = cli_path();
    let out = Command::new("node")
        .arg(&cli)
        .args(args)
        .output()
        .map_err(|e| eyre!("failed to spawn `node {}`: {e}", cli.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(eyre!(
            "tv-mcp `{}` failed ({}): {}",
            args.join(" "),
            out.status,
            stderr.trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_granularities() {
        assert_eq!(tv_resolution("m15").unwrap(), "15");
        assert_eq!(tv_resolution("h1").unwrap(), "60");
        assert_eq!(tv_resolution("h4").unwrap(), "240");
        assert_eq!(tv_resolution("d").unwrap(), "1D");
        assert!(tv_resolution("nonsense").is_err());
    }

    #[test]
    fn resolves_symbol_with_broker_exchange_prefix() {
        // A TradeNation plan → TRADENATION: prefix (the bug that loaded OANDA).
        assert_eq!(
            tv_symbol("AUD/CHF", "tradenation").unwrap(),
            "TRADENATION:AUDCHF"
        );
        // An OANDA plan → OANDA: prefix.
        assert_eq!(tv_symbol("GBP/USD", "oanda").unwrap(), "OANDA:GBPUSD");
        // Unknown broker → bare symbol (TV's default exchange), no failure.
        assert_eq!(tv_symbol("GBP/USD", "").unwrap(), "GBPUSD");
    }

    #[test]
    fn resolves_symbol_when_catalog_lacks_tv_field() {
        // AUD/SGD is in instrument-lookup with an OANDA symbol but a blank
        // `tradingview` field, so `symbol_for(TradingView)` is None. The
        // fallback strips the OANDA underscore → OANDA:AUDSGD (not AUD_SGD).
        assert_eq!(tv_symbol("AUD_SGD", "oanda").unwrap(), "OANDA:AUDSGD");
    }

    #[test]
    fn strips_broker_separators() {
        assert_eq!(strip_separators("AUD_SGD"), "AUDSGD");
        assert_eq!(strip_separators("AUD/SGD"), "AUDSGD");
        assert_eq!(strip_separators("Spot Gold"), "SpotGold");
        assert_eq!(strip_separators("EURUSD"), "EURUSD");
    }

    #[test]
    fn exchange_prefix_map() {
        assert_eq!(tv_exchange("tradenation"), Some("TRADENATION"));
        assert_eq!(tv_exchange("OANDA"), Some("OANDA"));
        assert_eq!(tv_exchange("mystery"), None);
    }

    /// Parse from JSON text rather than building the struct by hand, so the test
    /// covers the serde wiring (field names, missing-field defaults) too — that's
    /// where a real break would be.
    fn state(json: &str) -> ChartState {
        serde_json::from_str(json).expect("test JSON must parse")
    }

    /// A verbatim `tv state` response, captured from the live chart on
    /// 2026-08-04 (trimmed `studies`). Pins the real field names and the shapes
    /// of `symbol`/`resolution` — the whole short-circuit rests on these being
    /// the same forms `tv_symbol`/`tv_resolution` emit.
    const REAL_STATE: &str = r#"{
        "success": true,
        "symbol": "TRADENATION:EURCAD",
        "resolution": "240",
        "chartType": 1,
        "studies": [{"id": "9xBjuz", "name": "Inefficient Candle Highlighter"}]
    }"#;

    /// THE case this feature exists for: the chart is already on the plan's
    /// symbol + timeframe, so no load is needed. Uses the real captured payload
    /// and the values our own mappers produce for that plan.
    #[test]
    fn already_on_the_chart_matches() {
        let symbol = tv_symbol("EUR/CAD", "tradenation").unwrap();
        let resolution = tv_resolution("h4").unwrap();
        assert_eq!(symbol, "TRADENATION:EURCAD", "mapper drifted from tv state");
        assert_eq!(resolution, "240", "mapper drifted from tv state");
        assert!(state_matches(&state(REAL_STATE), &symbol, &resolution));
    }

    /// A different instrument must NOT short-circuit — this is the expensive
    /// mistake (operator left on the wrong chart, and tv-arm re-arms from
    /// whatever is up, so it's a wrong answer not a slow one).
    #[test]
    fn different_symbol_does_not_match() {
        assert!(!state_matches(
            &state(REAL_STATE),
            "TRADENATION:EURUSD",
            "240"
        ));
    }

    /// Right instrument, wrong timeframe — must still load.
    #[test]
    fn different_resolution_does_not_match() {
        assert!(!state_matches(
            &state(REAL_STATE),
            "TRADENATION:EURCAD",
            "60"
        ));
    }

    /// Same pair on the OTHER broker's exchange is a different chart. This is the
    /// bug the exchange prefix exists to prevent, so the fast path must not
    /// re-open it by comparing bare symbols.
    #[test]
    fn same_pair_on_another_exchange_does_not_match() {
        assert!(!state_matches(&state(REAL_STATE), "OANDA:EURCAD", "240"));
    }

    /// `success: false` means the read is untrustworthy → load anyway.
    #[test]
    fn unsuccessful_state_does_not_match() {
        let json = r#"{"success": false, "symbol": "TRADENATION:EURCAD", "resolution": "240"}"#;
        assert!(!state_matches(&state(json), "TRADENATION:EURCAD", "240"));
    }

    /// Missing fields (an upstream shape change) → load anyway, never skip.
    #[test]
    fn missing_fields_do_not_match() {
        let no_symbol = r#"{"success": true, "resolution": "240"}"#;
        assert!(!state_matches(
            &state(no_symbol),
            "TRADENATION:EURCAD",
            "240"
        ));
        let no_resolution = r#"{"success": true, "symbol": "TRADENATION:EURCAD"}"#;
        assert!(!state_matches(
            &state(no_resolution),
            "TRADENATION:EURCAD",
            "240"
        ));
        let empty = r#"{}"#;
        assert!(!state_matches(&state(empty), "TRADENATION:EURCAD", "240"));
    }

    /// Unknown extra fields must not break parsing — `tv state` already returns
    /// `chartType`/`studies`, and more may appear.
    #[test]
    fn unknown_fields_are_ignored() {
        let json = r#"{"success": true, "symbol": "OANDA:GBPUSD",
                       "resolution": "1D", "somethingNew": {"a": 1}}"#;
        assert!(state_matches(&state(json), "OANDA:GBPUSD", "1D"));
    }

    /// Casing differences are not a reason to reload: TradingView symbols are
    /// case-insensitive, and `1D`/`1d` are the same resolution.
    #[test]
    fn comparison_ignores_case() {
        let json = r#"{"success": true, "symbol": "oanda:gbpusd", "resolution": "1d"}"#;
        assert!(state_matches(&state(json), "OANDA:GBPUSD", "1D"));
    }

    /// TradingView arms from the LIVE chart, so it has no spec-url — passing
    /// none is exactly the pre-existing behaviour, unchanged.
    #[test]
    fn tradingview_backend_has_no_spec_url() {
        assert_eq!(
            ChartBackend::TradingView.spec_url("EUR/CAD", "oanda", "h1"),
            None,
            "TradingView must keep reading the live chart"
        );
    }

    /// local-chart arms from its own `/arm-setup`. THE regression test for the
    /// `--new-tv` split-brain: before this, `l` navigated local-chart and the
    /// replay armed off a stale TradingView tab.
    #[test]
    fn local_chart_backend_arms_from_its_own_endpoint() {
        let backend = ChartBackend::LocalChart {
            base_url: "http://127.0.0.1:8790".to_string(),
        };
        assert_eq!(
            backend.spec_url("EUR/CAD", "oanda", "h1").as_deref(),
            Some("http://127.0.0.1:8790/arm-setup?instrument=EUR_CAD&tf=h1&broker=oanda")
        );
    }

    /// `load_chart_backend` takes the goto and gives it to local-chart.
    ///
    /// Asserted at the DISPATCH signature rather than by driving a browser
    /// open: the TradingView arm shells `node`, so a test that actually
    /// called it would launch processes. What matters here is that the
    /// parameter exists and reaches the local-chart arm — the URL it builds
    /// is pinned by `local_chart`'s own `build_url` tests, and the end-to-end
    /// behaviour by local-chart's `test_goto_url_param.py`.
    #[test]
    fn load_chart_backend_accepts_a_goto_for_local_chart() {
        let _: fn(&ChartBackend, &str, &str, &str, Option<&str>) -> Result<bool> =
            load_chart_backend;
    }

    /// The raw replay's chart URL: present on local-chart, absent on
    /// TradingView.
    ///
    /// `R` drives `replay-candles` directly on the stored plan, so it has no
    /// tv-arm to route through and needs the URL itself. A mutation returning
    /// `None` unconditionally would silently put the local-chart backend back
    /// on TradingView's `--annotate` — the exact coupling this closes.
    #[test]
    fn only_the_local_chart_backend_yields_a_raw_replay_url() {
        assert_eq!(ChartBackend::TradingView.local_chart_url(), None);
        let local = ChartBackend::LocalChart {
            base_url: "http://127.0.0.1:8790".to_string(),
        };
        assert_eq!(local.local_chart_url(), Some("http://127.0.0.1:8790"));
    }

    /// The broker reaches the arm URL through the dispatch, and CHANGES it.
    ///
    /// The regression this pins: `spec_url` used to take only
    /// instrument+granularity, so a TradeNation plan armed off OANDA's
    /// drawings — local-chart keys drawings per broker and defaults a missing
    /// `broker=` to OANDA. The symptom was a `422` listing all four required
    /// roles, which reads as "nothing is drawn" rather than "wrong broker".
    #[test]
    fn the_broker_reaches_the_local_chart_arm_url() {
        let backend = ChartBackend::LocalChart {
            base_url: "http://127.0.0.1:8790".to_string(),
        };
        let oanda = backend.spec_url("EU50_EUR", "oanda", "h1");
        let tn = backend.spec_url("EU50_EUR", "tradenation", "h1");
        assert_ne!(oanda, tn, "the broker must change the arm URL");
        assert!(tn.unwrap_or_default().contains("broker=tradenation"));
    }

    /// The two backends must not agree: a mutation collapsing the match arms
    /// (either direction) is what this catches — the exact shape of the bug.
    #[test]
    fn the_two_backends_do_not_resolve_to_the_same_arm_source() {
        let local = ChartBackend::LocalChart {
            base_url: "http://127.0.0.1:8790".to_string(),
        };
        assert_ne!(
            ChartBackend::TradingView.spec_url("EUR/CAD", "oanda", "h1"),
            local.spec_url("EUR/CAD", "oanda", "h1"),
            "the backends must arm from different chart sources"
        );
    }
}
