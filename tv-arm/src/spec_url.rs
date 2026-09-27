//! `--spec-url` accepts a **pasted local-chart browser URL**, not just the
//! `/arm-setup` endpoint it fetches from.
//!
//! The operator is looking at a chart:
//!
//! ```text
//! http://127.0.0.1:8790/?instrument=GBP_JPY&tf=h4&broker=tradenation&goto=2026-08-24T08%3A17%3A35Z
//! ```
//!
//! and wants to arm off exactly those drawings. The endpoint that serves them
//! is the same host with a different path:
//!
//! ```text
//! http://127.0.0.1:8790/arm-setup?instrument=GBP_JPY&tf=h4&broker=tradenation
//! ```
//!
//! ## Why translate rather than tell the operator to edit it
//!
//! The two URLs carry the *same* chart identity — broker + instrument +
//! timeframe — so the edit is pure transcription, and transcription is where
//! this particular mistake is silent. Drop `broker=` on the way across and
//! local-chart defaults it to OANDA (`ChartBroker::from_param(None)`): a
//! TradeNation plan arms off OANDA's drawings. Best case that's a
//! `422 missing required roles` naming all four, which reads as "you forgot
//! the neckline" while the neckline sits on the TradeNation chart. Worst case
//! the OANDA chart *has* a stale line in range and the arm succeeds on the
//! wrong setup.
//!
//! So the conversion happens here, once, where it can be tested — not in the
//! operator's head every time.
//!
//! `goto` is dropped deliberately: it is a *view* hint (where to scroll the
//! browser), not part of chart identity. `/arm-setup` classifies the drawings
//! on the chart, and the arm cursor comes from `--start` / the spec, never
//! from where the operator happened to be looking.

use color_eyre::eyre::{Result, WrapErr, eyre};
use url::Url;

/// The path that serves a frozen setup. A URL already pointing here is passed
/// through untouched — `journal` builds exactly this form
/// (`journal/src/tv/local_chart.rs::arm_setup_url`), and normalising it again
/// would be a second derivation to drift from.
const ARM_SETUP_PATH: &str = "/arm-setup";

/// The query params that identify a chart. Everything else a browser URL
/// carries (`goto`, and any future view state) is a hint about what to *show*,
/// not about which drawings to classify.
const IDENTITY_PARAMS: [&str; 3] = ["instrument", "tf", "broker"];

/// Normalise a `--spec-url` value: accept either local-chart's `/arm-setup`
/// endpoint or a pasted chart URL from the browser's address bar, and return
/// the `/arm-setup` form to fetch.
///
/// An `/arm-setup` URL is returned verbatim. Anything else keeps the scheme,
/// host and port, takes the path to `/arm-setup`, and keeps only the
/// chart-identity params (see [`IDENTITY_PARAMS`]) in that fixed order — so the
/// same chart always produces the same URL regardless of how the browser
/// happened to order them.
pub fn normalise(raw: &str) -> Result<String> {
    let parsed = Url::parse(raw).wrap_err_with(|| format!("--spec-url {raw:?} is not a URL"))?;
    if parsed.path() == ARM_SETUP_PATH {
        return Ok(raw.to_string());
    }

    let identity = identity_params(&parsed);
    if identity.is_empty() {
        return Err(eyre!(
            "--spec-url {raw:?} names no chart: expected local-chart's \
             /arm-setup endpoint, or a chart URL carrying instrument / tf / \
             broker (as the browser's address bar shows, e.g. \
             'http://127.0.0.1:8790/?instrument=GBP_JPY&tf=h4&broker=tradenation')"
        ));
    }

    let mut converted = parsed.clone();
    converted.set_path(ARM_SETUP_PATH);
    // Replace the query wholesale rather than removing `goto` by name: an
    // allow-list means a param local-chart adds later cannot leak into an arm
    // URL and change what is fetched.
    converted.set_fragment(None);
    converted.query_pairs_mut().clear().extend_pairs(identity);
    Ok(converted.to_string())
}

/// The chart-identity params present in `url`, in [`IDENTITY_PARAMS`] order.
///
/// A param the URL does not carry is simply absent — `/arm-setup` has its own
/// defaults and its own 4xx for what it cannot do without, and duplicating
/// that judgement here would give two places to disagree about what a chart
/// needs. The one thing this *does* judge is "no identity params at all",
/// which is the paste-the-wrong-thing case and worth naming (see
/// [`normalise`]).
fn identity_params(url: &Url) -> Vec<(String, String)> {
    IDENTITY_PARAMS
        .iter()
        .filter_map(|name| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| ((*name).to_string(), value.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operator's actual paste, from the browser address bar.
    const CHART_URL: &str = "http://127.0.0.1:8790/?instrument=GBP_JPY&tf=h4\
        &broker=tradenation&goto=2026-08-24T08%3A17%3A35Z";

    #[test]
    fn a_pasted_chart_url_becomes_the_arm_setup_endpoint() {
        assert_eq!(
            normalise(CHART_URL).expect("a chart URL converts"),
            "http://127.0.0.1:8790/arm-setup?instrument=GBP_JPY&tf=h4&broker=tradenation"
        );
    }

    /// `goto` says where to scroll, not which chart. Carrying it across would
    /// hand `/arm-setup` a param it does not read — harmless today, and a
    /// silent behaviour change the day it starts reading it.
    #[test]
    fn goto_is_dropped() {
        let converted = normalise(CHART_URL).expect("converts");
        assert!(
            !converted.contains("goto"),
            "goto is a view hint, not chart identity: {converted}"
        );
    }

    /// The load-bearing one. `broker` is part of the DRAWINGS' identity —
    /// local-chart keys them `drawings/<broker>/<symbol>-<tf>.json` — so
    /// losing it in translation arms a TradeNation plan off OANDA's chart.
    #[test]
    fn broker_survives_the_conversion() {
        let converted = normalise(CHART_URL).expect("converts");
        assert!(
            converted.contains("broker=tradenation"),
            "dropping broker arms off OANDA's drawings: {converted}"
        );
    }

    /// journal builds the `/arm-setup` form itself
    /// (`journal/src/tv/local_chart.rs::arm_setup_url`). Every existing caller
    /// must keep working byte-identically, so this path is not re-derived.
    #[test]
    fn an_arm_setup_url_is_passed_through_unchanged() {
        let already = "http://127.0.0.1:8790/arm-setup?instrument=EUR_CAD&tf=h1&broker=oanda";
        assert_eq!(normalise(already).expect("passes through"), already);
    }

    /// Including one with no broker: normalising it would not *add* one, and
    /// rewriting it at all risks reordering params under a caller that built
    /// them deliberately.
    #[test]
    fn an_arm_setup_url_without_a_broker_is_still_untouched() {
        let already = "http://127.0.0.1:8790/arm-setup?instrument=EUR_CAD&tf=h1";
        assert_eq!(normalise(already).expect("passes through"), already);
    }

    /// Param order in the address bar is the browser's business. The arm URL
    /// is canonical so the same chart always fetches the same URL.
    #[test]
    fn identity_params_come_out_in_a_fixed_order() {
        let shuffled = "http://127.0.0.1:8790/?broker=tradenation&tf=h4&instrument=GBP_JPY";
        assert_eq!(
            normalise(shuffled).expect("converts"),
            "http://127.0.0.1:8790/arm-setup?instrument=GBP_JPY&tf=h4&broker=tradenation"
        );
    }

    /// A non-default port is the isolated-server case (a second local-chart on
    /// :8815 with its own `--data-dir`), and losing it would arm off the
    /// operator's real drawings.
    #[test]
    fn a_non_default_port_is_kept() {
        let other = "http://127.0.0.1:8815/?instrument=EUR_USD&tf=h1&broker=oanda";
        assert_eq!(
            normalise(other).expect("converts"),
            "http://127.0.0.1:8815/arm-setup?instrument=EUR_USD&tf=h1&broker=oanda"
        );
    }

    /// A chart URL with no query at all is a paste of the wrong thing (the
    /// bare server root). Fetching `/arm-setup` bare would arm off whatever
    /// chart local-chart defaults to, which is the silent-wrong-setup failure
    /// this whole module exists to avoid.
    #[test]
    fn a_url_naming_no_chart_is_refused() {
        let err = normalise("http://127.0.0.1:8790/")
            .expect_err("a bare root names no chart")
            .to_string();
        assert!(
            err.contains("instrument") && err.contains("names no chart"),
            "the error should say what is missing: {err}"
        );
    }

    #[test]
    fn a_non_url_is_refused_as_a_url() {
        let err = normalise("not a url").expect_err("not a URL").to_string();
        assert!(err.contains("not a URL"), "{err}");
    }

    /// `/arm-setup` is matched on the PATH, so a chart URL that merely
    /// mentions it in a param is still converted rather than fetched as-is.
    #[test]
    fn arm_setup_is_matched_on_the_path_not_the_whole_string() {
        let sneaky = "http://127.0.0.1:8790/?instrument=GBP_JPY&tf=h4&broker=arm-setup";
        assert_eq!(
            normalise(sneaky).expect("converts"),
            "http://127.0.0.1:8790/arm-setup?instrument=GBP_JPY&tf=h4&broker=arm-setup"
        );
    }
}
