//! Point the local-chart app (a browser tab already open on it) at a plan's
//! instrument + timeframe, via the URL-param bootstrap
//! `?instrument=<id>&tf=<granularity>` added to `local-chart`'s own
//! `static/index.html` (branch `feat/new-tv-url-bootstrap` in that repo).
//!
//! ## Why this has no already-there short-circuit
//!
//! [`super::load_chart`] (TradingView) reads the live chart's state first
//! (`tv state`) and skips a redundant load — see that module's doc comment.
//! local-chart has **no server-readable "what's currently on screen"**: the
//! instrument box and timeframe are plain in-page JS state with nothing
//! persisted server-side (confirmed: `local-chart/src/http.rs` has no
//! per-viewer state at all, and `static/index.html` reads no URL params
//! except the bootstrap this module drives one-way). So there is no honest
//! way to answer "is it already there" without asking the browser itself
//! (which this TUI cannot do — it only shells a URL opener).
//!
//! [`load_chart_local`] therefore **always** opens the URL and always
//! returns `Ok(false)` ("did load"). This is a *known, deliberate*
//! asymmetry from the TradingView path, and it sits on the SAFE side of the
//! same fail-open rule that path documents: the cost of an unconditional
//! reload here is a redundant browser navigation (the page re-fetches
//! candles, drawings, news — a few hundred ms to a couple of seconds on
//! localhost), never a stranded operator looking at the wrong chart. Never
//! "simplify" this into claiming `Ok(true)` on some heuristic (e.g. "same
//! trade_id as last time") — that fabricates a fact this process cannot
//! observe, which is exactly the wrong direction to be wrong in.

use color_eyre::eyre::Result;
use tracing::warn;

use instrument_lookup::{Broker, by_broker_symbol};

/// Default local-chart URL, matching its own `DEFAULT_PORT` (`src/args.rs`).
pub const DEFAULT_LOCAL_CHART_URL: &str = "http://127.0.0.1:8790";

/// Open local-chart at `base_url`, navigated to `instrument`'s OANDA-style id
/// and `granularity`, optionally centred on `goto`. `base_url` has no trailing
/// slash. Always returns `Ok(false)` — see the module doc for why there is no
/// already-there short-circuit here. Only a failure to **launch the opener
/// itself** is an `Err`; an unresolvable instrument or unrecognised
/// granularity still opens the chart (falling back to the bare instrument / no
/// `tf` param) rather than refusing outright — the fail-open direction from
/// `tv.rs`, applied here too: doubt about the mapping is not a reason to leave
/// the operator with no action at all.
///
/// `goto` is an RFC3339 **UTC** instant — in practice a plan's `armed_at`, so
/// the chart lands on the arm bar instead of the operator hunting for the
/// setup. It is passed through verbatim; local-chart's own bootstrap parses
/// it and ignores anything it cannot read as a UTC instant. Same fail-open
/// direction: a bad timestamp costs the centring, never the navigation.
pub fn load_chart_local(
    base_url: &str,
    instrument: &str,
    granularity: &str,
    goto: Option<&str>,
) -> Result<bool> {
    load_chart_local_with(base_url, instrument, granularity, goto, crate::opener::open)
}

/// [`load_chart_local`] with the actual browser-open call injected, so a test
/// can observe the exact URL that WOULD be opened without spawning a real
/// browser (`crate::opener::open` fires a detached `xdg-open`/`gio` child —
/// fine in production, unwanted side effect from a test suite). Mirrors the
/// "prefer a seam you can observe over actually driving the real program"
/// rule this whole feature was asked to follow for the TradingView path too.
fn load_chart_local_with(
    base_url: &str,
    instrument: &str,
    granularity: &str,
    goto: Option<&str>,
    opener: impl FnOnce(&str) -> Result<&'static str>,
) -> Result<bool> {
    let symbol = local_chart_symbol(instrument);
    let url = build_url(base_url, &symbol, granularity, goto);
    opener(&url)?;
    Ok(false)
}

/// Resolve a plan instrument id to local-chart's own convention: a bare
/// OANDA-style symbol (`EUR_USD`), **never** exchange-qualified — unlike
/// [`super::tv_symbol`], local-chart is single-broker (OANDA-fed) and its
/// `#instrument` box holds exactly this form (`local-chart/src/symbol.rs`'s
/// `resolve_symbol` doc comment: "The chart's instrument box... use
/// `EUR_USD`").
///
/// Tries both broker views of the raw id (a TradeNation-form id like
/// `AUD/CHF` needs to resolve too, even though local-chart only *trades*
/// OANDA-fed data) and falls back to stripping separators when the catalog
/// has no OANDA form for this asset — the same last-resort `tv.rs` uses, so
/// an unknown/OANDA-less instrument still opens *something* rather than
/// nothing.
fn local_chart_symbol(instrument: &str) -> String {
    [Broker::Oanda, Broker::TradeNation]
        .into_iter()
        .find_map(|b| {
            by_broker_symbol(b, instrument)
                .ok()
                .flatten()
                .and_then(|asset| asset.symbol_for(Broker::Oanda))
                .map(str::to_string)
        })
        .unwrap_or_else(|| super::strip_separators(instrument))
}

/// local-chart's accepted timeframe tokens (`local-chart/src/granularity.rs`
/// `Timeframe::parse`) happen to be the SAME lowercase strings the plan's own
/// `granularity` field already carries (`m15`/`h1`/`h4`/`d`/`w`/`m`) — no
/// TradingView-style remapping needed, unlike [`super::tv_resolution`]. Still
/// validated against the known set rather than passed through blind: an
/// unrecognised token is dropped from the URL (loud in the log, and the chart
/// falls back to its own default) rather than sent as-is to a query param
/// whose failure mode on the browser side is silent (local-chart's `tf`
/// bootstrap simply finds no matching button and no-ops the whole pair).
const LOCAL_CHART_TIMEFRAMES: &[&str] = &["m15", "h1", "h4", "d", "w", "m"];

fn local_chart_tf(granularity: &str) -> Option<String> {
    let g = granularity.to_ascii_lowercase();
    LOCAL_CHART_TIMEFRAMES.contains(&g.as_str()).then_some(g)
}

/// Build `<base_url>/?instrument=<symbol>[&tf=<granularity>][&goto=<instant>]`.
/// The `tf` param is DROPPED (not defaulted to something) when `granularity`
/// doesn't map — dropping only its own param rather than refusing the whole
/// navigation: the operator still lands on the right instrument, just not the
/// right timeframe, and the gap is visible (the chart shows its own
/// last/default timeframe, not a fabricated match).
///
/// `goto` is appended only when present, so every existing link is
/// byte-identical to before it existed. It is percent-encoded: an RFC3339
/// instant carries `:` throughout and may carry `+` in its offset, and a raw
/// `+` in a query string decodes to a SPACE — which would silently corrupt
/// the timestamp rather than fail.
fn build_url(base_url: &str, symbol: &str, granularity: &str, goto: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    let mut url = match local_chart_tf(granularity) {
        Some(tf) => format!("{base}/?instrument={symbol}&tf={tf}"),
        None => {
            warn!(
                "local-chart: unrecognised granularity {granularity:?} — opening \
                 {symbol} with no `tf` param (chart keeps its own default)"
            );
            format!("{base}/?instrument={symbol}")
        }
    };
    if let Some(instant) = goto {
        url.push_str("&goto=");
        url.push_str(&percent_encode_query(instant));
    }
    url
}

/// Percent-encode a query-parameter VALUE, keeping only the unreserved set
/// (`A-Z a-z 0-9 - . _ ~`) literal.
///
/// Hand-rolled rather than pulling a dependency in for one call: `journal` is
/// a small TUI whose only other URL construction is the two params above,
/// both of which are already restricted alphabets. The one input that is not
/// is this timestamp.
///
/// Encoding every reserved character rather than an allow-list of "the ones a
/// timestamp has" means a future caller passing something less tame cannot
/// produce a malformed URL.
fn percent_encode_query(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Build the `--spec-url` for this plan: local-chart's `GET /arm-setup`, which
/// emits tv-arm's own `FrozenSetup` shape (`tv-arm/src/frozen_setup.rs`). This
/// is what lets a replay/fixture capture under `--new-tv` read the LOCAL-CHART
/// drawings instead of falling through to whatever TradingView happens to be
/// showing.
///
/// Reuses [`local_chart_symbol`] and [`local_chart_tf`] so the arm reads the
/// **same** instrument+timeframe the `l` key just navigated to — one derivation,
/// not a second one to drift from [`build_url`].
///
/// ## Why an unknown granularity is `None` here, and only a dropped param there
///
/// [`build_url`] drops a bad `tf` and still navigates: the operator lands on the
/// right instrument at the chart's own default timeframe, and can see that it's
/// wrong. Arming has no such visible gap — `/arm-setup` would classify the
/// drawings of whatever timeframe local-chart defaulted to and hand back a
/// setup that looks perfectly valid at the WRONG granularity, which is then
/// frozen into a replay or a fixture. So this returns `None` and the caller
/// falls back to the live-chart arm rather than silently arming off-timeframe.
pub fn arm_setup_url(base_url: &str, instrument: &str, granularity: &str) -> Option<String> {
    let symbol = local_chart_symbol(instrument);
    let tf = local_chart_tf(granularity).or_else(|| {
        warn!(
            "local-chart: unrecognised granularity {granularity:?} for {symbol} — cannot \
             build an /arm-setup URL (arming off an unknown timeframe would freeze the \
             wrong geometry); falling back to the live-chart arm"
        );
        None
    })?;
    let base = base_url.trim_end_matches('/');
    Some(format!("{base}/arm-setup?instrument={symbol}&tf={tf}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `--spec-url` points at `/arm-setup` with the SAME instrument+tf
    /// mapping the navigation URL uses — a replay must arm off the chart the
    /// `l` key just loaded, not a differently-derived one.
    #[test]
    fn arm_setup_url_matches_the_navigation_mapping() {
        let url = arm_setup_url("http://127.0.0.1:8790", "EUR/CAD", "h1");
        assert_eq!(
            url.as_deref(),
            Some("http://127.0.0.1:8790/arm-setup?instrument=EUR_CAD&tf=h1")
        );
        // Same symbol resolution as the navigation path, for the same input.
        assert!(
            build_url(
                "http://127.0.0.1:8790",
                &local_chart_symbol("EUR/CAD"),
                "h1",
                None
            )
            .contains("instrument=EUR_CAD")
        );
    }

    /// Composite-key pair, half one: instrument varies, timeframe fixed.
    #[test]
    fn arm_setup_url_varies_with_instrument_granularity_fixed() {
        let eur = arm_setup_url("http://127.0.0.1:8790", "EUR_USD", "h4");
        let gbp = arm_setup_url("http://127.0.0.1:8790", "GBP_USD", "h4");
        assert_ne!(eur, gbp, "different instrument must change the URL");
        assert!(eur.unwrap_or_default().contains("tf=h4"));
    }

    /// Composite-key pair, half two: timeframe varies, instrument fixed. Catches
    /// a mutation that drops `tf` from the arm URL, which the half above cannot
    /// see. See the repo memory `composite_key_tests_must_vary_each_half`.
    #[test]
    fn arm_setup_url_varies_with_granularity_instrument_fixed() {
        let h4 = arm_setup_url("http://127.0.0.1:8790", "EUR_USD", "h4");
        let m15 = arm_setup_url("http://127.0.0.1:8790", "EUR_USD", "m15");
        assert_ne!(h4, m15, "different granularity must change the URL");
        assert!(h4.unwrap_or_default().contains("tf=h4"));
        assert!(m15.unwrap_or_default().contains("tf=m15"));
    }

    /// The deliberate asymmetry from [`build_url`]: navigation drops a bad `tf`
    /// and still opens, but arming REFUSES. A dropped `tf` here would arm off
    /// local-chart's default timeframe and freeze geometry that looks valid but
    /// is read at the wrong granularity — invisible in the resulting plan.
    #[test]
    fn arm_setup_url_refuses_an_unknown_granularity_rather_than_dropping_tf() {
        assert_eq!(
            arm_setup_url("http://127.0.0.1:8790", "EUR_USD", "not-a-tf"),
            None
        );
        // Contrast: the navigation URL still opens, minus the tf param.
        assert_eq!(
            build_url("http://127.0.0.1:8790", "EUR_USD", "not-a-tf", None),
            "http://127.0.0.1:8790/?instrument=EUR_USD"
        );
    }

    #[test]
    fn arm_setup_url_strips_a_trailing_slash_from_the_base_url() {
        assert_eq!(
            arm_setup_url("http://127.0.0.1:8790/", "EUR_USD", "h1").as_deref(),
            Some("http://127.0.0.1:8790/arm-setup?instrument=EUR_USD&tf=h1")
        );
    }

    #[test]
    fn maps_granularity_tokens_unchanged() {
        // Unlike TradingView's remapped resolutions, local-chart's own tokens
        // ARE the plan's granularity string, lowercased.
        assert_eq!(local_chart_tf("h4").as_deref(), Some("h4"));
        assert_eq!(local_chart_tf("M15").as_deref(), Some("m15"));
        assert_eq!(local_chart_tf("d").as_deref(), Some("d"));
        assert_eq!(local_chart_tf("nonsense"), None);
    }

    #[test]
    fn resolves_symbol_to_oanda_form_never_exchange_qualified() {
        // TradeNation-form input still resolves, and — unlike tv_symbol,
        // which strips separators for a bare TradingView symbol — this
        // targets OANDA's OWN form, which keeps the underscore (`AUD_CHF`,
        // matching local-chart's `#instrument` box convention). NEVER
        // exchange-prefixed either way: local-chart is single-broker.
        assert_eq!(local_chart_symbol("AUD/CHF"), "AUD_CHF");
        assert_eq!(local_chart_symbol("EUR_USD"), "EUR_USD");
    }

    #[test]
    fn falls_back_to_stripped_separators_when_unresolvable() {
        assert_eq!(local_chart_symbol("MADE_UP_XYZ"), "MADEUPXYZ");
    }

    #[test]
    fn builds_url_with_both_params_when_granularity_is_known() {
        let url = build_url("http://127.0.0.1:8790", "EURUSD", "h4", None);
        assert_eq!(url, "http://127.0.0.1:8790/?instrument=EURUSD&tf=h4");
    }

    /// Composite-key trap: this test varies ONLY the instrument while the
    /// granularity stays the well-known "h4" — on its own it cannot catch a
    /// mutation that drops `tf=` from the URL, because the fixed half never
    /// fails. Paired with the next test, which fixes the instrument and
    /// varies the granularity, so BOTH halves are independently exercised.
    #[test]
    fn url_varies_with_instrument_granularity_fixed() {
        let eur = build_url("http://127.0.0.1:8790", "EURUSD", "h4", None);
        let gbp = build_url("http://127.0.0.1:8790", "GBPUSD", "h4", None);
        assert_ne!(
            eur, gbp,
            "different instrument must produce a different URL"
        );
        assert!(eur.contains("tf=h4") && gbp.contains("tf=h4"));
    }

    /// The other half of the composite-key pair: instrument FIXED, timeframe
    /// varied. A mutation that silently drops the timeframe from the mapping
    /// (e.g. `local_chart_tf` always returning `None`, or `build_url` never
    /// reading its `granularity` argument) leaves the instrument-only test
    /// above green but makes every URL here identical — this is what catches
    /// it. See the repo memory note
    /// `composite_key_tests_must_vary_each_half`.
    #[test]
    fn url_varies_with_granularity_instrument_fixed() {
        let h4 = build_url("http://127.0.0.1:8790", "EURUSD", "h4", None);
        let m15 = build_url("http://127.0.0.1:8790", "EURUSD", "m15", None);
        let d = build_url("http://127.0.0.1:8790", "EURUSD", "d", None);
        assert_ne!(
            h4, m15,
            "different granularity must produce a different URL"
        );
        assert_ne!(m15, d);
        assert_ne!(h4, d);
        assert!(h4.contains("tf=h4") && m15.contains("tf=m15") && d.contains("tf=d"));
    }

    #[test]
    fn drops_only_the_tf_param_on_an_unknown_granularity() {
        let url = build_url("http://127.0.0.1:8790", "EURUSD", "not-a-real-tf", None);
        assert_eq!(url, "http://127.0.0.1:8790/?instrument=EURUSD");
        assert!(
            !url.contains("tf="),
            "unknown granularity must not leak through"
        );
    }

    #[test]
    fn strips_a_trailing_slash_from_the_base_url() {
        let url = build_url("http://127.0.0.1:8790/", "EURUSD", "h4", None);
        assert_eq!(url, "http://127.0.0.1:8790/?instrument=EURUSD&tf=h4");
    }

    /// THE central contract this module exists to document: unlike the
    /// TradingView path, a successful local-chart load is ALWAYS `Ok(false)`
    /// ("did load"), never `Ok(true)` ("already there") — there is no
    /// server-readable "what's on screen" to compare against, so claiming
    /// `true` would be fabricating a fact. Observed via the injectable-opener
    /// seam rather than a real browser spawn.
    #[test]
    fn a_successful_open_is_always_ok_false_never_already_there() {
        let opened = std::cell::RefCell::new(None);
        let result = load_chart_local_with("http://127.0.0.1:8790", "EUR_USD", "h4", None, |url| {
            *opened.borrow_mut() = Some(url.to_string());
            Ok("xdg-open")
        });
        assert!(!result.unwrap(), "must NEVER report already-there");
        assert_eq!(
            opened.into_inner().as_deref(),
            Some("http://127.0.0.1:8790/?instrument=EUR_USD&tf=h4")
        );
    }

    /// End-to-end against a REAL running local-chart instance (ignored by
    /// default — run with `cargo test -p journal -- --ignored
    /// local_chart::e2e`, pointed at a throwaway instance via
    /// `LOCAL_CHART_E2E_URL`, e.g. `http://127.0.0.1:8824`, never the
    /// operator's own `:8790`). Drives the REAL, non-mocked mapping code
    /// (`local_chart_symbol` through `instrument-lookup`'s live catalog,
    /// `local_chart_tf`, `build_url`) for a realistic TradeNation-form plan
    /// instrument, captures the exact URL via the injectable-opener seam
    /// (never `xdg-open` — this sandbox has no display server to observe a
    /// spawned GUI browser), then confirms the URL is reachable and returns
    /// the chart shell — i.e. the same URL a browser would actually load.
    /// The URL's CONTENT (does `?instrument=&tf=` actually drive the page to
    /// the right instrument/timeframe) is proven separately with headless
    /// Playwright against the same running instance — see the commit message
    /// on `feat/new-tv-url-bootstrap` in the local-chart repo for that run's
    /// output. This test's job is narrower and still real: prove the URL
    /// journal's shipped code builds is the one that gets served, for an
    /// instrument that only resolves through a real catalog lookup.
    #[test]
    #[ignore]
    fn e2e_real_url_against_a_running_local_chart_instance() {
        let base_url = std::env::var("LOCAL_CHART_E2E_URL")
            .expect("set LOCAL_CHART_E2E_URL to a throwaway local-chart instance, e.g. :8824");

        let mut captured_url = None;
        let result = load_chart_local_with(&base_url, "AUD/CHF", "h4", None, |url| {
            captured_url = Some(url.to_string());
            Ok("test-capture")
        });
        assert!(result.is_ok());
        let url = captured_url.expect("opener must have been called");
        // Real catalog resolution: AUD/CHF (TradeNation form) -> AUD_CHF
        // (OANDA form) -- not a hardcoded/mocked answer.
        assert_eq!(url, format!("{base_url}/?instrument=AUD_CHF&tf=h4"));

        // Confirm the URL is actually reachable and serves the chart shell —
        // the same bytes a browser opening it would receive.
        let body = ureq_get(&url).expect("GET the real URL against the running instance");
        assert!(
            body.contains("id=\"chart\""),
            "served page must be the chart shell"
        );
    }

    /// Minimal blocking HTTP GET with no new dependency — `journal` has no
    /// HTTP client today, and this is a single ignored e2e test, not
    /// production code. Uses `std::net::TcpStream` directly against the
    /// already-parsed `host:port` from a `http://` URL (the only shape this
    /// test ever calls with).
    fn ureq_get(url: &str) -> color_eyre::eyre::Result<String> {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| color_eyre::eyre::eyre!("test helper only supports http://"))?;
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let mut stream = TcpStream::connect(authority)?;
        let request =
            format!("GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes())?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response)
    }

    /// The one honest `Err`: the opener itself failed to launch (no
    /// xdg-open/gio on the system). Doubt about the URL MAPPING still opens
    /// something (see the unknown-granularity/unresolvable-instrument tests
    /// above) — only a total inability to launch anything is an error.
    #[test]
    fn only_a_launch_failure_is_an_error() {
        let result =
            load_chart_local_with("http://127.0.0.1:8790", "EUR_USD", "h4", None, |_url| {
                Err(color_eyre::eyre::eyre!("no opener on this system"))
            });
        assert!(result.is_err());
    }

    /// Pins the public wrapper's signature — the one `tv::load_chart_backend`
    /// actually calls — so a refactor of the injectable-opener seam cannot
    /// silently change what callers see.
    #[test]
    fn public_wrapper_has_the_documented_signature() {
        let _: fn(&str, &str, &str, Option<&str>) -> Result<bool> = load_chart_local;
    }

    /// A plan's `armed_at` rides along as `&goto=`, so the chart CENTRES on
    /// the arm bar rather than opening at its right edge.
    #[test]
    fn a_goto_instant_is_appended_to_the_url() {
        let url = build_url(
            "http://127.0.0.1:8790",
            "EUR_CAD",
            "h1",
            Some("2026-09-02T11:19:54Z"),
        );
        assert!(url.contains("goto="), "goto forwarded: {url}");
        // The instrument/tf half is untouched by the addition.
        assert!(
            url.contains("instrument=EUR_CAD") && url.contains("tf=h1"),
            "{url}"
        );
    }

    /// The colon is percent-encoded. A raw RFC3339 instant in a query string
    /// is not merely ugly — `+` in an offset decodes to a SPACE, which
    /// corrupts the timestamp silently instead of failing.
    #[test]
    fn a_goto_instant_is_percent_encoded() {
        let url = build_url(
            "http://127.0.0.1:8790",
            "EUR_CAD",
            "h1",
            Some("2026-09-02T11:19:54Z"),
        );
        assert!(
            url.ends_with("&goto=2026-09-02T11%3A19%3A54Z"),
            "colons must be encoded: {url}"
        );
        assert!(!url.contains("11:19:54"), "a raw colon leaked: {url}");
        // The nanosecond form a real plan carries round-trips too.
        let nanos = build_url(
            "http://127.0.0.1:8790",
            "EUR_CAD",
            "h1",
            Some("2026-09-02T11:19:54.201124894Z"),
        );
        assert!(nanos.contains("54.201124894Z"), "{nanos}");
    }

    /// `+` is the one that MUST encode: raw, a query parser reads it as a
    /// space, so an offset-bearing instant would arrive mangled rather than
    /// rejected. Pinned separately because the common case (a `Z` instant)
    /// contains no `+` at all and so cannot catch it.
    #[test]
    fn a_plus_in_an_offset_is_encoded_not_left_to_become_a_space() {
        let url = build_url(
            "http://127.0.0.1:8790",
            "EUR_CAD",
            "h1",
            Some("2026-09-02T21:19:54+10:00"),
        );
        assert!(url.contains("%2B"), "the + must be encoded: {url}");
        assert!(!url.contains('+'), "a raw + leaked: {url}");
    }

    /// No `armed_at` (or the TradingView backend) means no param at all —
    /// every pre-existing link is byte-identical to before goto existed.
    #[test]
    fn no_goto_leaves_the_url_exactly_as_it_was() {
        let url = build_url("http://127.0.0.1:8790", "EUR_CAD", "h1", None);
        assert_eq!(url, "http://127.0.0.1:8790/?instrument=EUR_CAD&tf=h1");
        assert!(!url.contains("goto"), "{url}");
    }

    /// The goto rides on the degraded URL too. An unrecognised granularity
    /// drops only `tf` (see [`build_url`]), and the centring is independent of
    /// that — dropping both would lose the operator's position for an
    /// unrelated reason.
    #[test]
    fn a_goto_survives_an_unknown_granularity() {
        let url = build_url(
            "http://127.0.0.1:8790",
            "EUR_CAD",
            "not-a-tf",
            Some("2026-09-02T11:19:54Z"),
        );
        assert!(!url.contains("tf="), "{url}");
        assert!(url.contains("goto=2026-09-02T11%3A19%3A54Z"), "{url}");
    }

    /// The encoder keeps the unreserved set literal and encodes everything
    /// else, so a future caller passing something less tame than a timestamp
    /// still produces a well-formed URL.
    #[test]
    fn percent_encoding_keeps_unreserved_characters_literal() {
        assert_eq!(percent_encode_query("aZ09-._~"), "aZ09-._~");
        assert_eq!(percent_encode_query("a b&c=d"), "a%20b%26c%3Dd");
        // Non-ASCII encodes per UTF-8 byte, not per char.
        assert_eq!(percent_encode_query("é"), "%C3%A9");
    }
}
