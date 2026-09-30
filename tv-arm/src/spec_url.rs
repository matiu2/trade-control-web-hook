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
//!
//! ## `mode` — which spec, decided by the subcommand
//!
//! `/arm-setup` requires `mode=register` or `mode=replay` and answers a 400
//! without it. The two return **different specs from the same chart**: a chart
//! keeps its history, so after the operator draws the next live setup the old
//! replay's `start` note, fib and `trade-expiry` line are all still on it.
//!
//! - `register` takes the newest drawings, ignores the `start` note, and the
//!   spec carries **no** `start` (so tv-arm arms at wall-clock now). A spec
//!   whose `trade-expiry` is already past is a 422.
//! - `replay` needs the `start` note and carries it, taking the drawings that
//!   belong to it.
//!
//! The server refuses to guess because guessing is how a live `register` armed
//! off a replay-chosen spec — a three-week-old `trade-expiry` and a stale
//! `start` — and expired on its first tick. tv-arm must not guess either, and
//! it doesn't have to: the subcommand being run already says which one it is
//! ([`SpecMode::for_command`]). The mode is added to **both** input forms, a
//! pasted chart URL and a ready-made `/arm-setup` URL (journal builds those
//! without a mode). An `/arm-setup` URL that already names the *other* mode is
//! refused rather than overridden: it is exactly the cross-wiring above,
//! spelled out by the caller.
//!
//! ## `start` — a replay's `--start`, forwarded
//!
//! A replay spec needs a start instant. `/arm-setup` finds one from a `start`
//! note on the chart, or — winning over the note — a `start=<epoch>` param. A
//! journal replay (`tv-arm --spec-url <arm-setup url> --start <t> ... replay`)
//! draws no note, so without the param it gets a 422; and a stale note left by
//! another trade must not beat the instant the caller named. So in replay mode
//! [`with_start`] forwards `--start` as `start=`, on both input forms. Register
//! mode never sends one: a register spec carries no start by design, and
//! local-chart answers a 400 to `mode=register&start=`.

use color_eyre::eyre::{Result, WrapErr, eyre};
use url::Url;

use crate::args::Command;

/// The path that serves a frozen setup. A URL already pointing here keeps
/// every param it carries, in its order — `journal` builds exactly this form
/// (`journal/src/tv/local_chart.rs::arm_setup_url`), and re-deriving it would
/// be a second derivation to drift from. The only thing added is `mode`
/// (see [`SpecMode`]).
const ARM_SETUP_PATH: &str = "/arm-setup";

/// The query params that identify a chart. Everything else a browser URL
/// carries (`goto`, and any future view state) is a hint about what to *show*,
/// not about which drawings to classify.
const IDENTITY_PARAMS: [&str; 3] = ["instrument", "tf", "broker"];

/// The query param naming which spec `/arm-setup` should export.
const MODE_PARAM: &str = "mode";

/// The query param carrying a replay's start instant, epoch seconds.
const START_PARAM: &str = "start";

/// Which spec `/arm-setup` exports: the latest drawings for a live arm, or the
/// drawings at the chart's `start` note for a replay. See the module docs for
/// why the caller has to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecMode {
    /// A new live trade: newest drawings, `start` note ignored.
    Register,
    /// One past setup, located by the chart's `start` note.
    Replay,
}

impl SpecMode {
    /// The `mode=` value local-chart accepts.
    pub fn as_param(self) -> &'static str {
        match self {
            Self::Register => "register",
            Self::Replay => "replay",
        }
    }

    /// The mode a subcommand fetches its spec in.
    ///
    /// - `register` arms a live trade → [`SpecMode::Register`].
    /// - `replay` re-runs a setup through `replay-candles` → [`SpecMode::Replay`].
    /// - `plan-out` writes the plan for the offline `replay-candles` harness,
    ///   and like `replay` it needs a cursor (a register spec carries none) →
    ///   [`SpecMode::Replay`].
    /// - no subcommand builds a bundle to disk with no stated purpose, so there
    ///   is no honest answer: an error naming the subcommands, not a guess.
    pub fn for_command(command: Option<&Command>) -> Result<Self> {
        match command {
            Some(Command::Register { .. }) => Ok(Self::Register),
            Some(Command::Replay { .. } | Command::PlanOut { .. }) => Ok(Self::Replay),
            None => Err(eyre!(
                "--spec-url needs a subcommand to know which spec to fetch: \
                 `register` (a live arm: the chart's newest drawings) or \
                 `replay` / `plan-out` (the drawings at the chart's `start` \
                 note). local-chart's /arm-setup will not guess between them, \
                 and neither will tv-arm"
            )),
        }
    }
}

/// Normalise a `--spec-url` value: accept either local-chart's `/arm-setup`
/// endpoint or a pasted chart URL from the browser's address bar, and return
/// the `/arm-setup` form to fetch, carrying `mode` (see [`SpecMode`]).
///
/// An `/arm-setup` URL keeps its params as given and gains `mode` when it has
/// none; one naming a different mode is an error. Anything else keeps the
/// scheme, host and port, takes the path to `/arm-setup`, and keeps only the
/// chart-identity params (see [`IDENTITY_PARAMS`]) in that fixed order, then
/// `mode` — so the same chart always produces the same URL regardless of how
/// the browser happened to order them.
pub fn normalise(raw: &str, mode: SpecMode) -> Result<String> {
    let parsed = Url::parse(raw).wrap_err_with(|| format!("--spec-url {raw:?} is not a URL"))?;
    if parsed.path() == ARM_SETUP_PATH {
        return with_mode(raw, parsed, mode);
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
    converted
        .query_pairs_mut()
        .clear()
        .extend_pairs(identity)
        .append_pair(MODE_PARAM, mode.as_param());
    Ok(converted.to_string())
}

/// An `/arm-setup` URL with `mode` settled: kept when it already matches,
/// appended when absent, refused when it names anything else.
///
/// Refused rather than overridden: a caller that wrote `mode=replay` and is
/// now running `register` has cross-wired the two, and silently fetching the
/// other spec would hide that from the one person who can fix it.
fn with_mode(raw: &str, mut parsed: Url, mode: SpecMode) -> Result<String> {
    let given: Vec<String> = parsed
        .query_pairs()
        .filter(|(key, _)| key == MODE_PARAM)
        .map(|(_, value)| value.into_owned())
        .collect();
    if let Some(other) = given.iter().find(|value| *value != mode.as_param()) {
        return Err(eyre!(
            "--spec-url {raw:?} asks for mode={other}, but this subcommand \
             arms a {wanted} spec (mode={wanted}). A register spec is the \
             chart's newest drawings with no `start`; a replay spec is the \
             drawings at the `start` note — arming one as the other is how a \
             live plan expired on its first tick. Drop `mode` from the URL \
             (tv-arm adds the right one) or run the matching subcommand",
            wanted = mode.as_param()
        ));
    }
    if given.is_empty() {
        parsed
            .query_pairs_mut()
            .append_pair(MODE_PARAM, mode.as_param());
        return Ok(parsed.to_string());
    }
    Ok(raw.to_string())
}

/// A normalised `/arm-setup` URL with a replay's `--start` added as
/// `start=<epoch>` (see the module docs). `start` is `--start` already parsed
/// to epoch seconds (`pipeline::parse_start`).
///
/// Unchanged in register mode or without `--start`. A URL that already carries
/// the same `start` is kept byte-identical; a different one is refused, for
/// the same reason a conflicting `mode` is: two start instants for one replay
/// is a cross-wiring the caller has to settle, not tv-arm.
pub fn with_start(url: &str, mode: SpecMode, start: Option<i64>) -> Result<String> {
    let Some(start) = start.filter(|_| mode == SpecMode::Replay) else {
        return Ok(url.to_string());
    };
    let mut parsed = Url::parse(url).wrap_err_with(|| format!("spec URL {url:?} is not a URL"))?;
    let given: Vec<String> = parsed
        .query_pairs()
        .filter(|(key, _)| key == START_PARAM)
        .map(|(_, value)| value.into_owned())
        .collect();
    if let Some(other) = given
        .iter()
        .find(|value| value.parse::<i64>().ok() != Some(start))
    {
        return Err(eyre!(
            "--spec-url {url:?} carries start={other}, but --start is {start} \
             (epoch seconds). One replay has one start: drop `start` from the \
             URL (tv-arm adds --start's) or make them agree"
        ));
    }
    if given.is_empty() {
        parsed
            .query_pairs_mut()
            .append_pair(START_PARAM, &start.to_string());
        return Ok(parsed.to_string());
    }
    Ok(url.to_string())
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
    use crate::args::Args;
    use clap::Parser;

    /// The operator's actual paste, from the browser address bar.
    const CHART_URL: &str = "http://127.0.0.1:8790/?instrument=GBP_JPY&tf=h4\
        &broker=tradenation&goto=2026-08-24T08%3A17%3A35Z";

    /// What journal builds (`journal/src/tv/local_chart.rs::arm_setup_url`):
    /// the endpoint, with no mode.
    const ARM_SETUP_URL: &str =
        "http://127.0.0.1:8790/arm-setup?instrument=EUR_CAD&tf=h1&broker=oanda";

    fn register(raw: &str) -> Result<String> {
        normalise(raw, SpecMode::Register)
    }

    #[test]
    fn a_pasted_chart_url_becomes_the_arm_setup_endpoint() {
        assert_eq!(
            register(CHART_URL).expect("a chart URL converts"),
            "http://127.0.0.1:8790/arm-setup?instrument=GBP_JPY&tf=h4&broker=tradenation\
             &mode=register"
        );
    }

    /// The same chart, run under `replay`, fetches the replay spec. Without
    /// the mode the server answers 400; with the wrong one it hands back the
    /// other trade's drawings.
    #[test]
    fn a_pasted_chart_url_under_replay_asks_for_the_replay_spec() {
        assert_eq!(
            normalise(CHART_URL, SpecMode::Replay).expect("converts"),
            "http://127.0.0.1:8790/arm-setup?instrument=GBP_JPY&tf=h4&broker=tradenation\
             &mode=replay"
        );
    }

    /// `goto` says where to scroll, not which chart. Carrying it across would
    /// hand `/arm-setup` a param it does not read — harmless today, and a
    /// silent behaviour change the day it starts reading it.
    #[test]
    fn goto_is_dropped() {
        let converted = register(CHART_URL).expect("converts");
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
        let converted = register(CHART_URL).expect("converts");
        assert!(
            converted.contains("broker=tradenation"),
            "dropping broker arms off OANDA's drawings: {converted}"
        );
    }

    /// journal builds the `/arm-setup` form itself with no mode. Its params
    /// are kept as built, in their order, and the mode is appended — journal
    /// needs no change to keep working against the mode-requiring endpoint.
    #[test]
    fn an_arm_setup_url_without_a_mode_gets_one_appended() {
        assert_eq!(
            register(ARM_SETUP_URL).expect("gains a mode"),
            format!("{ARM_SETUP_URL}&mode=register")
        );
        assert_eq!(
            normalise(ARM_SETUP_URL, SpecMode::Replay).expect("gains a mode"),
            format!("{ARM_SETUP_URL}&mode=replay")
        );
    }

    /// An `/arm-setup` URL is never re-derived from the allow-list: a param
    /// the caller put there deliberately (here a viewport) survives.
    #[test]
    fn an_arm_setup_url_keeps_params_outside_the_identity_allow_list() {
        let with_viewport = "http://127.0.0.1:8790/arm-setup?instrument=EUR_CAD&tf=h1\
                             &from=1700000000&to=1700090000";
        assert_eq!(
            normalise(with_viewport, SpecMode::Replay).expect("gains a mode"),
            format!("{with_viewport}&mode=replay")
        );
    }

    /// Including one with no broker: normalising it would not *add* one, and
    /// rewriting it at all risks reordering params under a caller that built
    /// them deliberately.
    #[test]
    fn an_arm_setup_url_without_a_broker_gains_only_the_mode() {
        let already = "http://127.0.0.1:8790/arm-setup?instrument=EUR_CAD&tf=h1";
        assert_eq!(
            register(already).expect("gains a mode"),
            format!("{already}&mode=register")
        );
    }

    /// A caller that already named the right mode gets its URL back
    /// byte-identical — not a second `mode`.
    #[test]
    fn a_matching_mode_is_kept_as_is() {
        let already = format!("{ARM_SETUP_URL}&mode=register");
        assert_eq!(register(&already).expect("passes through"), already);

        let replay = "http://127.0.0.1:8790/arm-setup?mode=replay&instrument=EUR_CAD&tf=h1";
        assert_eq!(
            normalise(replay, SpecMode::Replay).expect("passes through"),
            replay
        );
    }

    /// The bug itself, spelled out by a caller: a live `register` handed a
    /// replay spec URL. Overriding would hide the cross-wiring; fetching as
    /// given would arm the stale trade. Neither — refuse, naming both modes.
    #[test]
    fn a_conflicting_mode_is_refused_not_overridden() {
        let replay_url = format!("{ARM_SETUP_URL}&mode=replay");
        let err = register(&replay_url)
            .expect_err("register must not fetch a replay spec")
            .to_string();
        assert!(
            err.contains("mode=replay") && err.contains("mode=register"),
            "the error should name both modes: {err}"
        );

        let register_url = format!("{ARM_SETUP_URL}&mode=register");
        let err = normalise(&register_url, SpecMode::Replay)
            .expect_err("replay must not fetch a register spec")
            .to_string();
        assert!(err.contains("mode=register"), "{err}");
    }

    /// An unknown value is a conflict too — tv-arm cannot vouch for what the
    /// server would do with it.
    #[test]
    fn an_unknown_mode_is_refused() {
        let odd = format!("{ARM_SETUP_URL}&mode=live");
        let err = register(&odd).expect_err("unknown mode").to_string();
        assert!(err.contains("mode=live"), "{err}");
    }

    /// Param order in the address bar is the browser's business. The arm URL
    /// is canonical so the same chart always fetches the same URL.
    #[test]
    fn identity_params_come_out_in_a_fixed_order() {
        let shuffled = "http://127.0.0.1:8790/?broker=tradenation&tf=h4&instrument=GBP_JPY";
        assert_eq!(
            register(shuffled).expect("converts"),
            "http://127.0.0.1:8790/arm-setup?instrument=GBP_JPY&tf=h4&broker=tradenation\
             &mode=register"
        );
    }

    /// A mode on a pasted chart URL is view state like any other non-identity
    /// param: the allow-list drops it, and the subcommand's mode is used.
    #[test]
    fn a_mode_on_a_pasted_chart_url_is_replaced_by_the_subcommands() {
        let pasted = "http://127.0.0.1:8790/?instrument=GBP_JPY&tf=h4&mode=replay";
        assert_eq!(
            register(pasted).expect("converts"),
            "http://127.0.0.1:8790/arm-setup?instrument=GBP_JPY&tf=h4&mode=register"
        );
    }

    /// A non-default port is the isolated-server case (a second local-chart on
    /// :8815 with its own `--data-dir`), and losing it would arm off the
    /// operator's real drawings.
    #[test]
    fn a_non_default_port_is_kept() {
        let other = "http://127.0.0.1:8815/?instrument=EUR_USD&tf=h1&broker=oanda";
        assert_eq!(
            register(other).expect("converts"),
            "http://127.0.0.1:8815/arm-setup?instrument=EUR_USD&tf=h1&broker=oanda&mode=register"
        );
    }

    /// A chart URL with no query at all is a paste of the wrong thing (the
    /// bare server root). Fetching `/arm-setup` bare would arm off whatever
    /// chart local-chart defaults to, which is the silent-wrong-setup failure
    /// this whole module exists to avoid.
    #[test]
    fn a_url_naming_no_chart_is_refused() {
        let err = register("http://127.0.0.1:8790/")
            .expect_err("a bare root names no chart")
            .to_string();
        assert!(
            err.contains("instrument") && err.contains("names no chart"),
            "the error should say what is missing: {err}"
        );
    }

    #[test]
    fn a_non_url_is_refused_as_a_url() {
        let err = register("not a url").expect_err("not a URL").to_string();
        assert!(err.contains("not a URL"), "{err}");
    }

    /// `/arm-setup` is matched on the PATH, so a chart URL that merely
    /// mentions it in a param is still converted rather than fetched as-is.
    #[test]
    fn arm_setup_is_matched_on_the_path_not_the_whole_string() {
        let sneaky = "http://127.0.0.1:8790/?instrument=GBP_JPY&tf=h4&broker=arm-setup";
        assert_eq!(
            register(sneaky).expect("converts"),
            "http://127.0.0.1:8790/arm-setup?instrument=GBP_JPY&tf=h4&broker=arm-setup\
             &mode=register"
        );
    }

    const START: i64 = 1_790_300_000;

    /// journal's replay: an `/arm-setup` URL plus `--start`, no note drawn.
    #[test]
    fn a_replay_forwards_start_on_an_arm_setup_url() {
        let url = normalise(ARM_SETUP_URL, SpecMode::Replay).expect("mode");
        assert_eq!(
            with_start(&url, SpecMode::Replay, Some(START)).expect("start"),
            format!("{ARM_SETUP_URL}&mode=replay&start={START}")
        );
    }

    #[test]
    fn a_replay_forwards_start_on_a_pasted_chart_url() {
        let url = normalise(CHART_URL, SpecMode::Replay).expect("mode");
        assert_eq!(
            with_start(&url, SpecMode::Replay, Some(START)).expect("start"),
            format!(
                "http://127.0.0.1:8790/arm-setup?instrument=GBP_JPY&tf=h4&broker=tradenation\
                 &mode=replay&start={START}"
            )
        );
    }

    /// A register spec carries no start by design; local-chart 400s one.
    #[test]
    fn a_register_never_sends_start() {
        let url = register(ARM_SETUP_URL).expect("mode");
        assert_eq!(
            with_start(&url, SpecMode::Register, Some(START)).expect("unchanged"),
            url
        );
    }

    #[test]
    fn no_start_leaves_the_url_alone() {
        let url = normalise(ARM_SETUP_URL, SpecMode::Replay).expect("mode");
        assert_eq!(
            with_start(&url, SpecMode::Replay, None).expect("unchanged"),
            url
        );
    }

    #[test]
    fn a_matching_start_is_kept_as_is() {
        let url = format!("{ARM_SETUP_URL}&start={START}&mode=replay");
        assert_eq!(
            with_start(&url, SpecMode::Replay, Some(START)).expect("kept"),
            url
        );
    }

    /// Two start instants for one replay: refused, naming both.
    #[test]
    fn a_conflicting_start_is_refused() {
        let url = format!("{ARM_SETUP_URL}&mode=replay&start=1700000000");
        let err = with_start(&url, SpecMode::Replay, Some(START))
            .expect_err("conflict")
            .to_string();
        assert!(
            err.contains("start=1700000000") && err.contains(&START.to_string()),
            "{err}"
        );
    }

    /// The allow-list drops a `start` on a pasted chart URL, so `--start` is
    /// the only one — no false conflict with view state.
    #[test]
    fn a_start_on_a_pasted_chart_url_is_not_a_conflict() {
        let pasted = "http://127.0.0.1:8790/?instrument=GBP_JPY&tf=h4&start=1700000000";
        let url = normalise(pasted, SpecMode::Replay).expect("mode");
        assert!(
            with_start(&url, SpecMode::Replay, Some(START))
                .expect("start")
                .ends_with(&format!("&mode=replay&start={START}"))
        );
    }

    fn mode_for(argv: &[&str]) -> Result<SpecMode> {
        let args = Args::try_parse_from(argv).expect("parse args");
        SpecMode::for_command(args.command.as_ref())
    }

    /// Each subcommand fetches the spec it arms, parsed through clap exactly
    /// as the operator types it.
    #[test]
    fn the_subcommand_decides_the_mode() {
        assert_eq!(
            mode_for(&["tv-arm", "register"]).expect("register"),
            SpecMode::Register
        );
        assert_eq!(
            mode_for(&["tv-arm", "register", "--replace", "--shadow"]).expect("register"),
            SpecMode::Register
        );
        assert_eq!(
            mode_for(&["tv-arm", "replay", "--annotate", "false"]).expect("replay"),
            SpecMode::Replay
        );
        assert_eq!(
            mode_for(&["tv-arm", "plan-out", "/tmp/plan.json"]).expect("plan-out"),
            SpecMode::Replay
        );
    }

    /// A bare invocation states no purpose, so there is nothing to pick the
    /// spec by. Guessing is the bug; the error names the choices instead.
    #[test]
    fn no_subcommand_is_refused_rather_than_guessed() {
        let err = mode_for(&["tv-arm"])
            .expect_err("no subcommand, no mode")
            .to_string();
        assert!(
            err.contains("register") && err.contains("replay"),
            "the error should name the subcommands to choose from: {err}"
        );
    }
}
