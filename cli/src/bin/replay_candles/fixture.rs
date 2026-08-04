//! Golden-file fixtures for `replay-candles`: freeze a known-good replay's
//! inputs (plan + the exact candle window) and its expected outcome to disk, so
//! a `cargo test` can re-run it **offline** and catch any future engine change
//! that silently moves the verdict on a verified scenario.
//!
//! A fixture is one self-contained directory `replay-fixtures/<name>/`:
//!
//! ```text
//! plan.json      — the TradePlan (input)
//! candles.json   — the pulled window, frozen so the fixture needs no broker
//! meta.json      — resolved scalars (instrument, granularity, source, window)
//! expected.json  — the golden ReplayOutcome snapshot: fires + what it EARNED
//! sub_bars.json  — OPTIONAL: the finer (e.g. M1) candles the sub-bar zoom
//!                  actually consulted on this run. Absent when the run had no
//!                  ambiguous SL/TP bar, which is the common case.
//! ```
//!
//! ## `sub_bars.json` — only the bars the zoom actually looked at
//!
//! An ambiguous exit bar (one whose range sweeps BOTH the stop and the target)
//! can only be resolved by finer candles. The live replay fetches those lazily
//! (see `super::lazy_zoom`): it runs the sim once serving nothing to learn WHICH
//! bars are ambiguous, fetches only those windows, then re-runs. `--save` freezes
//! exactly that fetched subset here, so the fixture reproduces its own verdict
//! offline instead of silently degrading to the pessimistic stop.
//!
//! Deliberately NOT the whole finer window: storing M1 across every fixture's
//! span would be ~416 MB for this corpus, versus ~1 MB for the bars the zoom can
//! actually consume (the exit loop returns on the first ambiguous bar, so each
//! entry zooms at most once).
//!
//! **The stored set is a function of the strategy that saved it.** Change the
//! bracket, the widen behaviour, or the entry rule and a *different* bar may go
//! ambiguous — one this fixture has no bars for. `lazy_zoom::FixtureSubBars`
//! detects that (a window outside the saved extent is a *miss*) and `run_frozen`
//! re-fetches it from the broker/candle-cache rather than silently scoring that
//! bar as a stop; a re-save then re-freezes the new set.
//!
//! The snapshot schema ([`ReplayOutcome`]) is owned here, not in the engine — it
//! captures exactly what the test should assert (each fire's decision and the
//! run's economics), independent of `report.rs`'s human-facing text.
//!
//! ## One fill path — do not add a second
//!
//! Economics live in exactly one place: **`outcome`** ([`ReplayEconomics`]) —
//! net R, counts, and per-position legs, booked by `report::render` off the
//! `ReplayBroker` held ledger (`fire.realized`). It is the same value the report
//! prints, passed in rather than recomputed.
//!
//! There used to be a second, independent view — `fires[].fill`, a per-fire
//! re-simulation via `fill_sim::simulate_fill`. It was **deleted on
//! 2026-07-27** and should not come back. It was not a useful second opinion;
//! it was confidently wrong in the same vocabulary as the right answer. Of the
//! five fills it recorded on the tracked `uk-100-…-close-on-reversal` fixture:
//!
//! - **two were phantom** — fills for superseded enters that never placed an
//!   order at all,
//! - **two were wrong** — a reversal-close reported as `stopped_out` at 0R
//!   (really +0.549R), and an expiry-close reported as `filled_open`, never
//!   resolved (really +0.797R). `simulate_fill` has no reversal- or
//!   expiry-close awareness, so it walks the bracket on past the bar the ledger
//!   actually flattened the position,
//! - **one agreed.**
//!
//! Scoring a grid off it would have read ≈−1R on a trade that made +0.35R.
//!
//! The one thing it knew that `outcome.legs` does not is the **not-taken** case:
//! whether a resting order would ever have triggered. That is now answered with
//! real broker data on the dedicated `not-taken` demo account rather than by
//! simulation — slower feedback, but true. Gate declines are separate again, and
//! stay: [`FireOutcome::suppressed_by`] records what the engine *observed*, not
//! what a simulator guessed.
//!
//! Note `fill_sim` itself is **not** dead — `replay_broker.rs` drives the held
//! ledger with `simulate_fill_resolved_zoom`, and that is the one authority.
//! What was removed is the fixture's *separate second call* into it.

use std::fs;
use std::path::Path;

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use trade_control_core::intent::Action;
use trade_control_engine::{BidAskCandle as EngineCandle, Granularity, TradePlan};

use super::arm_record::ArmRecord;
use super::economics::ReplayEconomics;
use super::replay::Replay;
use trade_control_cli::replay_args::CandleSource;

/// The resolved scalars a fixture replay needs, beyond the plan + candles. Saved
/// so `--test-mode` can reconstruct the run without re-resolving from flags, the
/// plan, or the TradingView chart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixtureMeta {
    /// Broker symbol the candles were pulled for (already source-resolved).
    pub instrument: String,
    /// Bar size — drives each tick's `now` in [`super::replay::run`].
    pub granularity: Granularity,
    /// Which broker the candles came from (recorded for provenance).
    pub source: CandleSource,
    /// Window start (UTC), as resolved at save time.
    pub start: DateTime<Utc>,
    /// Window end (UTC), as resolved at save time (the plan's trade-expiry etc).
    pub end: DateTime<Utc>,
    /// Free-text note from `--message` at save time: what this fixture is meant
    /// to model, so a future reader knows the intent if the golden ever breaks.
    /// Journalling only — never read back into the replay. Omitted from the JSON
    /// (and older fixtures load fine) when no message was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// **Which arming variant this fixture froze** — entry rule, calendar flag,
    /// versions, journal ref. See [`ArmRecord`].
    ///
    /// A fixture captures one flag combination; without this, six variants of the
    /// same trade are indistinguishable on disk except by filename. Journalling
    /// and grouping only — never read back into the replay. Omitted when the save
    /// carried no arm information (and pre-field fixtures load as `None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm: Option<ArmRecord>,
}

/// The golden snapshot of a replay: every fire's decision, what the run earned,
/// the terminal flag/phase, and the deduped warnings.
///
/// **Compare with [`super::golden_eq::outcome_matches`], not `==`.** The derived
/// `PartialEq` is bit-exact on `f64`, and the capture path (`--save`) and the
/// check path (`--check` / the fixture test) legitimately disagree in the last
/// bit or two — 2 ULP on a `stop_loss` red-flagged four EUR/USD cells on
/// 2026-07-30. `golden_eq` keeps every structural field exact and tolerances only
/// the measured floats. `PartialEq` is still derived because the
/// save→load round-trip test wants it (same value in and out, so bit equality is
/// the right predicate there).
///
/// `deny_unknown_fields` because serde's default is to **silently ignore** a key
/// it doesn't recognise, and that made the golden gate tolerate schema drift.
/// Caught deleting `fires[].fill` on 2026-07-27: the goldens still carried five
/// `"fill"` objects and every fixture test passed anyway, because the loader
/// quietly dropped them. A stale key is now a load error naming the field, which
/// is the prompt to re-bless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayOutcome {
    pub fires: Vec<FireOutcome>,
    pub done: bool,
    /// The terminal spine phase (serde snake_case, e.g. `done` / `await_entry`).
    pub final_phase: trade_control_engine::Phase,
    pub warnings: Vec<String>,
    /// What the run **earned**: net R, outcome counts, and the per-position legs.
    ///
    /// This is the number batch analysis actually wants — a 6-cell entry-rule ×
    /// news grid is six of these — and it hardens `--check`: before this field
    /// existed, a regression could silently halve Net R while firing exactly the
    /// same rules and still pass the golden gate.
    ///
    /// Booked by `report::render` (see [`ReplayEconomics`]), so the printed
    /// `Net R:` line and this field are the same computation, not two.
    ///
    /// `None` when the run had `--simulate` off — nothing was booked, matching
    /// the report, which prints no summary in that mode. `#[serde(default)]` so
    /// fixtures saved before this field existed still load; they read as `None`
    /// until re-blessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ReplayEconomics>,
}

/// One fired intent's decision: which rule, what action, on which bar.
///
/// Deliberately carries **no fill or economic field** — what a position earned
/// is `ReplayOutcome::outcome` and nothing else (see the module doc's "one fill
/// path"). Everything here is an *observation* of what the engine decided, so
/// there is no second computation to drift.
///
/// `deny_unknown_fields` for the reason on [`ReplayOutcome`] — and with extra
/// force here, since this is the struct a re-added `fill` would land on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FireOutcome {
    pub rule_id: String,
    /// The intent's action (serde kebab-case, e.g. `enter` / `veto`).
    pub action: Action,
    /// Open-time of the triggering candle.
    pub candle_time: DateTime<Utc>,
    /// Close of the triggering candle.
    pub candle_close: f64,
    /// Active news-blackout ids that suppressed this enter (paused at fire
    /// time). Empty/omitted for any fire the blackout gate let through. A
    /// suppressed enter books no leg — it's a 0R skip. Serialized so a fixture
    /// freezes the with-blackout SKIP as a regression.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suppressed_by: Vec<String>,
}

impl ReplayOutcome {
    /// Build the golden snapshot from a completed [`Replay`]. `simulate` mirrors
    /// the run's `--simulate` flag (off → no economics, matching the report).
    ///
    /// `economics` is what `report::render` booked for this same replay. It is
    /// **passed in rather than recomputed** on purpose: booking it twice is how
    /// the report and the golden diverged in the first place (see the module
    /// doc). Pass `None` for a non-simulated run.
    ///
    /// Note there is deliberately no `plan` parameter. It used to be here only to
    /// re-simulate each fire's fill a second way; that path is gone.
    pub fn compute(replay: &Replay, simulate: bool, economics: Option<&ReplayEconomics>) -> Self {
        let fires = replay
            .fires
            .iter()
            .map(|fire| FireOutcome {
                rule_id: fire.fired.rule_id.clone(),
                action: fire.fired.intent.action,
                candle_time: fire.fired.candle.time,
                candle_close: fire.fired.candle.c,
                suppressed_by: fire.suppressed_by(),
            })
            .collect();
        ReplayOutcome {
            fires,
            done: replay.done,
            final_phase: replay.final_state.phase,
            warnings: replay.warnings.clone(),
            // Economics only exist for a simulated run; without `--simulate`
            // there are no fills to book.
            outcome: simulate.then(|| economics.cloned()).flatten(),
        }
    }
}

const PLAN_FILE: &str = "plan.json";
const CANDLES_FILE: &str = "candles.json";
const META_FILE: &str = "meta.json";
const EXPECTED_FILE: &str = "expected.json";
const SUB_BARS_FILE: &str = "sub_bars.json";

/// Write a complete fixture to `dir` (created if absent): the plan, the frozen
/// candle window, the resolved meta, and the expected outcome — each as
/// pretty-printed JSON for readable diffs.
///
/// `sub_bars` is the finer series the zoom actually consulted (see the module
/// docs). It is written **only when non-empty**, so a fixture with no ambiguous
/// bar — the common case — keeps exactly the four files it always had and stays
/// byte-identical to one saved before sub-bar support existed.
pub fn save(
    dir: &Path,
    plan: &TradePlan,
    candles: &[EngineCandle],
    meta: &FixtureMeta,
    expected: &ReplayOutcome,
    sub_bars: &[EngineCandle],
) -> Result<()> {
    fs::create_dir_all(dir).wrap_err_with(|| format!("create fixture dir {}", dir.display()))?;
    write_json(&dir.join(PLAN_FILE), plan)?;
    write_json(&dir.join(CANDLES_FILE), &candles.to_vec())?;
    write_json(&dir.join(META_FILE), meta)?;
    write_json(&dir.join(EXPECTED_FILE), expected)?;
    let sub_bars_path = dir.join(SUB_BARS_FILE);
    if sub_bars.is_empty() {
        // Re-saving a fixture that used to have an ambiguous bar but no longer
        // does must not leave the old file behind — it would serve bars for a
        // window this run never asks about, and diverge from what `--save` says
        // it wrote.
        if sub_bars_path.exists() {
            fs::remove_file(&sub_bars_path)
                .wrap_err_with(|| format!("remove stale {}", sub_bars_path.display()))?;
        }
    } else {
        write_json(&sub_bars_path, &sub_bars.to_vec())?;
    }
    Ok(())
}

/// The frozen inputs of a fixture, loaded back for an offline replay.
pub struct FixtureInputs {
    pub plan: TradePlan,
    pub candles: Vec<EngineCandle>,
    pub meta: FixtureMeta,
    /// The finer candles the zoom consulted when this fixture was saved. Empty
    /// for a fixture with no ambiguous bar, and for every fixture saved before
    /// `sub_bars.json` existed — both of which replay exactly as they did then.
    pub sub_bars: Vec<EngineCandle>,
}

/// Read a fixture's inputs (plan + candles + meta + any saved sub-bars). The
/// expected outcome is read separately by the caller that needs it
/// ([`load_expected`]).
pub fn load(dir: &Path) -> Result<FixtureInputs> {
    let sub_bars_path = dir.join(SUB_BARS_FILE);
    Ok(FixtureInputs {
        plan: read_json(&dir.join(PLAN_FILE))?,
        candles: read_json(&dir.join(CANDLES_FILE))?,
        meta: read_json(&dir.join(META_FILE))?,
        // Absent is normal (no ambiguous bar / pre-sub-bar fixture), so it maps
        // to "none stored" — but a file that EXISTS and won't parse is a real
        // error, not an empty set. Silently treating corrupt sub-bars as absent
        // would degrade the zoom to the pessimistic stop with no signal.
        sub_bars: if sub_bars_path.exists() {
            read_json(&sub_bars_path)?
        } else {
            Vec::new()
        },
    })
}

/// Read a fixture's expected outcome from `dir`.
pub fn load_expected(dir: &Path) -> Result<ReplayOutcome> {
    read_json(&dir.join(EXPECTED_FILE))
}

/// Overwrite only a fixture's `expected.json` with a freshly-computed outcome,
/// leaving the frozen plan / candles / meta untouched. Used to **re-bless** a
/// fixture after an intended behaviour change (`--test-mode --rebless`).
pub fn save_expected(dir: &Path, expected: &ReplayOutcome) -> Result<()> {
    write_json(&dir.join(EXPECTED_FILE), expected)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let json = serde_json::to_string_pretty(value)
        .wrap_err_with(|| format!("serialize {}", path.display()))?;
    // Trailing newline: these files are committed, so without it every fixture
    // shows `\ No newline at end of file` in a diff and a re-bless produces noise
    // around the line that actually changed. Reading is unaffected — `serde_json`
    // ignores trailing whitespace — so this is comparison-safe both ways.
    fs::write(path, format!("{json}\n")).wrap_err_with(|| format!("write {}", path.display()))
}

/// Read + parse one of a fixture's JSON files.
///
/// The two failure modes are classified **differently on purpose**:
///
/// - A **parse** failure is `bad_input` (exit 4, "fix it"): the bytes on disk are
///   malformed, so retrying verbatim fails identically.
/// - A **read** failure is left untagged → infrastructure (exit 3, "retry it").
///   It is tempting to call a missing file bad input, but `ENOENT` is exactly what
///   a dropped NFS mount reports, and mis-tagging that as permanent silently drops
///   a result from a sweep. A genuinely absent fixture costs one wasted retry;
///   the other way costs a wrong answer. See `outcome::FailureKind::classify`.
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).wrap_err_with(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| {
        super::outcome::bad_input(color_eyre::eyre::eyre!("parse {}: {e}", path.display()))
    })
}

/// The repo-root fixtures directory, resolved from the cli crate's manifest so
/// the harness runs from any cwd: `<manifest>/../replay-fixtures`.
#[cfg(test)]
fn fixtures_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("replay-fixtures")
}

#[cfg(test)]
mod tests {
    use super::super::brisbane::bne;
    use super::*;
    use chrono::TimeZone;
    use std::path::PathBuf;
    use trade_control_cli::replay_args::{DetectorMarkConfig, DirectionFilter, GoldenFilter};

    /// The uk-100 multi-shot fixture, which several tests below need
    /// specifically: it's the one exercising a **trade-expiry** flatten on an
    /// open position, over multiple legs.
    ///
    /// It used to book a reversal-close on an open position too, which is why
    /// the test below was originally one test. Since the engulfer 2-bar span fix
    /// its stops sit wider, the first leg fills later, and both of its
    /// `close-on-reversal` fires now land while **flat** — they correctly book
    /// nothing. The reversal half of that coverage moved to
    /// [`XAU_XAG_REVERSAL`]; don't fold these back together without checking
    /// that one fixture still exercises both.
    const UK_100: &str = "uk-100-news-blackout-rentry-close-on-reversal";

    /// A reversal-close that lands on a genuinely **open** position and books a
    /// real leg. Carries the half of the close-booking guarantee that [`UK_100`]
    /// stopped exercising.
    const XAU_XAG_REVERSAL: &str = "xau-xag-close-on-reversal";

    /// The iH&S long whose `too-low` **invalidation** veto flattened it four days
    /// before the trade-expiry. Paired with [`UK_100`] (a genuine trade-expiry
    /// flatten) these two pin both sides of the `ClosePositions` classification.
    const GBP_NZD_INVALIDATION: &str = "gbp-nzd-h1-2026-07-22-normal-news-off";

    /// Resolve a fixture directory by name, **panicking** if it isn't there.
    ///
    /// These tests used to `eprintln!("… missing — skipping")` and return, so a
    /// renamed or deleted fixture turned them into vacuous passes — and `cargo
    /// test` hides the note unless you pass `--nocapture`. A test that reports
    /// `ok` when the thing it tests is gone is a false safety signal, which is
    /// worse than a missing test. Fail loudly and name the fixture.
    fn require_fixture(name: &str) -> PathBuf {
        let dir = super::fixtures_root().join(name);
        assert!(
            dir.is_dir(),
            "required fixture {name:?} is missing from {}. It was committed with the \
             corpus; restore it (or, if it was intentionally renamed, update this \
             test) — do not let this test pass without it.",
            super::fixtures_root().display()
        );
        dir
    }

    /// List the fixture directories under `replay-fixtures/` (each holding the
    /// four JSON files), sorted for deterministic test ordering. Empty when the
    /// dir is absent or has no sub-dirs — callers must treat empty as a failure,
    /// not a skip (see `all_fixtures_match_expected`).
    fn fixture_dirs() -> Vec<PathBuf> {
        let root = super::fixtures_root();
        let Ok(entries) = fs::read_dir(&root) else {
            return Vec::new();
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        dirs
    }

    /// The offline regression gate: every saved fixture re-runs through the pure
    /// engine and must reproduce its `expected.json`. No network, no env vars —
    /// frozen candles in, golden outcome out. A future engine change that moves a
    /// verified verdict fails here.
    ///
    /// **Panics on an empty corpus.** This used to `eprintln!` and return, which
    /// meant deleting or renaming `replay-fixtures/` made this test — and the
    /// three below — report `ok` in 0.00s with zero coverage, and `cargo test`
    /// swallows the note without `--nocapture`. A gate that goes green when its
    /// evidence disappears is worse than no gate: it actively reports safety.
    /// See `[[no_silent_degrade_prefer_loud_failure]]`.
    #[tokio::test]
    async fn all_fixtures_match_expected() {
        let dirs = fixture_dirs();
        assert!(
            !dirs.is_empty(),
            "no fixtures under {} — the golden gate has nothing to check. If you \
             genuinely intend an empty corpus, delete this test rather than letting \
             it pass vacuously; otherwise restore the directory (or save one with \
             `replay-candles … --save <name>`).",
            super::fixtures_root().display()
        );
        for dir in dirs {
            let name = dir.file_name().unwrap_or_default().to_string_lossy();
            let inputs = load(&dir).unwrap_or_else(|e| panic!("load fixture {name}: {e:?}"));
            let expected =
                load_expected(&dir).unwrap_or_else(|e| panic!("load expected for {name}: {e:?}"));

            // Far-future TTL so nothing expires mid-replay (mirrors run_frozen).
            let expires_at = inputs
                .candles
                .last()
                .map(|c| c.time)
                .unwrap_or_else(Utc::now)
                + chrono::Duration::days(365);
            // `live_start` is the saved window start: the frozen candles include
            // the warm-up prefix pulled before it, so the plan goes live at the
            // same boundary it did at save time.
            // Detector marking off (either axis `None`) — the fixture round-trip
            // compares the frozen outcome, which predates this feature and never
            // carried marks.
            let mark_cfg = DetectorMarkConfig::new(
                DirectionFilter::None,
                GoldenFilter::None,
                inputs.plan.direction,
            );
            let replay = super::super::replay::run(
                &inputs.plan,
                &inputs.candles,
                inputs.meta.granularity,
                inputs.meta.start,
                expires_at,
                mark_cfg,
                // The fixture's OWN saved finer candles (`sub_bars.json`), so a
                // fixture whose verdict needed a zoom reproduces it here. Empty
                // for a fixture with no ambiguous bar — the common case, and
                // every fixture saved before sub-bars existed — which replays
                // exactly as it always did (pessimistic stop).
                //
                // Deliberately NOT `FixtureSubBars` + refetch: this test runs
                // under `cargo test` with no credentials and no warm cache, so it
                // must stay fully offline. A fixture missing bars it now needs
                // shows up as a diverged golden here, which is the signal to
                // re-save it; the binary's `--test-mode` path is the one that
                // refetches.
                Some(Box::new(super::super::lazy_zoom::WindowSubBars::new(
                    inputs.sub_bars.clone(),
                ))),
            )
            .await;
            // Fixtures are saved from `--simulate` runs (the default), so the
            // golden outcome carries fills AND economics; recompute both the same
            // way the binary does — render (which books) then snapshot.
            let rendered =
                super::super::report::render(&inputs.plan, &replay, true, false, None, &mark_cfg);
            let computed = ReplayOutcome::compute(&replay, true, Some(&rendered.economics));

            // Tolerant compare (`golden_eq`), NOT `assert_eq!`: `ReplayOutcome`'s
            // derived `PartialEq` is bit-exact on floats, and the capture and
            // check paths legitimately differ by an ULP or two. Everything
            // structural is still exact.
            assert!(
                super::super::golden_eq::outcome_matches(&expected, &computed),
                "fixture {name} diverged:\n got: {}\n exp: {}",
                serde_json::to_string_pretty(&computed).unwrap_or_default(),
                serde_json::to_string_pretty(&expected).unwrap_or_default(),
            );
        }
    }

    /// S5 regression guard: the stateful broker books reversal- and expiry-close
    /// P&L from the held ledger, instead of the old no-op `close_positions` that
    /// left a reversal/expiry-closed position at 0R. The
    /// `uk-100-…-close-on-reversal` fixture exercises both a reversal-close and a
    /// trade-expiry flatten on OPEN positions.
    ///
    /// This used to grep the rendered REPORT TEXT, because the golden snapshot
    /// was computed via the independent `simulate_fill` path — which has no
    /// reversal/expiry awareness and so could not represent either outcome. Now
    /// that both consumers share [`ReplayEconomics`], the assertion is on the
    /// structured booking: counters and signed R, not substrings. The text
    /// rendering is still checked once, to catch a formatting regression that
    /// leaves the numbers right but stops showing them.
    #[tokio::test]
    async fn stateful_broker_books_expiry_closes_in_the_report() {
        let dir = require_fixture(UK_100);
        let inputs = load(&dir).expect("load uk-100 fixture");
        let expires_at = inputs
            .candles
            .last()
            .map(|c| c.time)
            .unwrap_or_else(Utc::now)
            + chrono::Duration::days(365);
        let mark_cfg = DetectorMarkConfig::new(
            DirectionFilter::None,
            GoldenFilter::None,
            inputs.plan.direction,
        );
        let replay = super::super::replay::run(
            &inputs.plan,
            &inputs.candles,
            inputs.meta.granularity,
            inputs.meta.start,
            expires_at,
            mark_cfg,
            None,
        )
        .await;
        let rendered =
            super::super::report::render(&inputs.plan, &replay, true, false, None, &mark_cfg);
        let econ = &rendered.economics;

        // The trade-expiry ClosePositions veto must FLATTEN the open position at
        // the expiry candle's close — the operator's original ask. Before S5 the
        // veto left the position open at 0R.
        assert_eq!(
            econ.expiry_closes, 1,
            "expiry veto must flatten the open position from the held ledger: {econ:#?}"
        );

        // The close is booked as a leg with a real exit and a signed R — the half
        // the old `simulate_fill` golden could not represent at all. This is the
        // assertion that guards Net R against a silent regression.
        let closed: Vec<_> = econ
            .legs
            .iter()
            .filter(|l| matches!(l.exit_reason, super::super::economics::ExitReason::Expiry))
            .collect();
        assert_eq!(
            closed.len(),
            1,
            "the expiry close must book a leg: {econ:#?}"
        );
        for leg in closed {
            assert!(
                leg.exit_price.is_some() && leg.exit_time.is_some(),
                "a flattened position must carry its exit: {leg:#?}"
            );
            assert!(
                leg.r.is_finite() && leg.r != 0.0,
                "a flattened position must book a signed R, not 0R: {leg:#?}"
            );
        }

        // The rendered text must still SHOW it (formatting regression guard).
        let text = &rendered.text;
        for want in ["CLOSED AT TRADE EXPIRY", "EXP: 1"] {
            assert!(text.contains(want), "report must show {want}:\n{text}");
        }
        // uk-100's flatten is the genuine `02-veto-trade-expiry`, so it must stay
        // on the expiry counter — this is the discriminator against the
        // invalidation close the same `ClosePositions` arm also dispatches. If a
        // future refactor keys the close reason off the veto LEVEL again, this
        // fires: the count moves to `invalidation_closes`.
        assert_eq!(
            econ.invalidation_closes, 0,
            "a trade-expiry flatten must not book as an invalidation close: {econ:#?}"
        );
        assert!(
            !text.contains("CLOSED ON INVALIDATION"),
            "a trade-expiry flatten must not be labelled an invalidation close:\n{text}"
        );

        // And the SNAPSHOT must carry the same economics — this is the half that
        // `--check` gates on, and the reason a silent Net R regression can no
        // longer pass while firing identical rules.
        let snapshot = ReplayOutcome::compute(&replay, true, Some(econ));
        assert_eq!(
            snapshot.outcome.as_ref(),
            Some(econ),
            "the golden snapshot must record the booked economics verbatim"
        );
    }

    /// The reversal half of the close-booking guarantee, on a fixture whose
    /// reversal-close lands on a genuinely **open** position. A reversal-close
    /// must book a real position off the held ledger, not sit at 0R / "still
    /// open" — before S5 `close_positions` was a no-op.
    ///
    /// Split out of the uk-100 test when the engulfer 2-bar span fix widened
    /// uk-100's stops enough that its reversal fires landed while flat (see
    /// [`UK_100`]). Keeping the assertion pinned to a fixture that still
    /// exercises it is the point — re-blessing it to `reversal_closes: 0` would
    /// have retired the coverage silently.
    #[tokio::test]
    async fn stateful_broker_books_a_reversal_close_on_an_open_position() {
        let dir = require_fixture(XAU_XAG_REVERSAL);
        let inputs = load(&dir).expect("load xau-xag fixture");
        let expires_at = inputs
            .candles
            .last()
            .map(|c| c.time)
            .unwrap_or_else(Utc::now)
            + chrono::Duration::days(365);
        let mark_cfg = DetectorMarkConfig::new(
            DirectionFilter::None,
            GoldenFilter::None,
            inputs.plan.direction,
        );
        let replay = super::super::replay::run(
            &inputs.plan,
            &inputs.candles,
            inputs.meta.granularity,
            inputs.meta.start,
            expires_at,
            mark_cfg,
            None,
        )
        .await;
        let rendered =
            super::super::report::render(&inputs.plan, &replay, true, false, None, &mark_cfg);
        let econ = &rendered.economics;

        assert_eq!(
            econ.reversal_closes, 1,
            "reversal-close must be booked from the held ledger: {econ:#?}"
        );
        let closed: Vec<_> = econ
            .legs
            .iter()
            .filter(|l| matches!(l.exit_reason, super::super::economics::ExitReason::Reversal))
            .collect();
        assert_eq!(
            closed.len(),
            1,
            "the reversal close must book a leg: {econ:#?}"
        );
        for leg in closed {
            assert!(
                leg.exit_price.is_some() && leg.exit_time.is_some(),
                "a flattened position must carry its exit: {leg:#?}"
            );
            assert!(
                leg.r.is_finite() && leg.r != 0.0,
                "a flattened position must book a signed R, not 0R: {leg:#?}"
            );
        }
        assert!(
            rendered.text.contains("CLOSED ON REVERSAL"),
            "report must show CLOSED ON REVERSAL:\n{}",
            rendered.text
        );
    }

    /// GBP/NZD iH&S 2026-07-22: a `too-low` invalidation veto flattened the
    /// position at 07-23 15:00 Brisbane, four days before the trade-expiry. Two
    /// bugs showed in the journal, and this gates both:
    ///
    /// 1. the exit was labelled `CLOSED AT EXPIRY` (the loop keyed the close
    ///    reason off `VetoLevel::ClosePositions`, which both vetos share), and
    /// 2. the journal kept narrating **stop management after the exit** — an
    ///    SL→break-even at 21:00 and a spread widen/restore the next day — because
    ///    the reconstruction helpers walked the whole replay window and stop only
    ///    at SL/TP, blind to a broker-side flatten.
    ///
    /// (2) is the half the golden `expected.json` can NOT catch: it records legs,
    /// not journal text, and the mislabelled run booked an identical −0.25R leg.
    /// So it has to be asserted on the rendered text, here.
    #[tokio::test]
    async fn an_invalidation_close_is_labelled_and_ends_the_journal() {
        let dir = require_fixture(GBP_NZD_INVALIDATION);
        let inputs = load(&dir).expect("load gbp-nzd fixture");
        let expires_at = inputs
            .candles
            .last()
            .map(|c| c.time)
            .unwrap_or_else(Utc::now)
            + chrono::Duration::days(365);
        let mark_cfg = DetectorMarkConfig::new(
            DirectionFilter::None,
            GoldenFilter::None,
            inputs.plan.direction,
        );
        let replay = super::super::replay::run(
            &inputs.plan,
            &inputs.candles,
            inputs.meta.granularity,
            inputs.meta.start,
            expires_at,
            mark_cfg,
            None,
        )
        .await;
        let rendered =
            super::super::report::render(&inputs.plan, &replay, true, false, None, &mark_cfg);
        let econ = &rendered.economics;
        let text = &rendered.text;

        // (1) The close is an invalidation, not an expiry — on both the counter
        // and the label. The trade-expiry is 07-27; nothing expired.
        assert_eq!(
            econ.invalidation_closes, 1,
            "the too-low flatten must book as an invalidation close: {econ:#?}"
        );
        assert_eq!(
            econ.expiry_closes, 0,
            "nothing expired — trade-expiry is 4 days after this close: {econ:#?}"
        );
        assert!(
            text.contains("CLOSED ON INVALIDATION"),
            "the exit must name the invalidation:\n{text}"
        );
        assert!(
            !text.contains("EXPIRY"),
            "the journal must not mention expiry at all for this trade:\n{text}"
        );

        // (2) Nothing may be narrated for this entry after it left the book.
        // Parse the exit time out of the journal rather than hardcoding an index,
        // so a reordering can't quietly make this vacuous.
        let leg = econ.legs.first().expect("one booked leg");
        let exit_at = leg.exit_time.expect("a flattened position has an exit");
        // `bne` renders "YYYY-MM-DD HH:MM:SS +10:00" — the same prefix the journal
        // lines start with, so a lexicographic compare on it orders by time.
        let exit_line = bne(exit_at)
            .get(..16)
            .expect("a Brisbane stamp is longer than 16 chars")
            .to_string();
        assert!(
            text.contains(&exit_line),
            "the exit bar {exit_line} must appear in the journal:\n{text}"
        );

        // Every stop-management line describes the live bracket, so each one dated
        // after the exit is a claim about a position that no longer existed.
        let stale: Vec<&str> = text
            .lines()
            .filter(|l| {
                l.contains("SL→break-even")
                    || l.contains("SL widened")
                    || l.contains("SL restored")
                    || l.contains("SL still widened")
            })
            .filter(|l| {
                // Journal lines start with the Brisbane bar time; anything sorting
                // after the exit's `YYYY-MM-DD HH:MM` prefix is post-exit.
                l.trim()
                    .get(..16)
                    .is_some_and(|stamp| stamp > exit_line.as_str())
            })
            .collect();
        assert!(
            stale.is_empty(),
            "stop management narrated after the {exit_line} exit — the position was \
             already flat:\n{stale:#?}\nfull journal:\n{text}"
        );
    }

    /// The printed `Net R:` line and the saved `outcome.net_r` are the same
    /// number, on a real fixture. They were two computations before
    /// `ReplayEconomics`; this pins them together.
    #[tokio::test]
    async fn saved_net_r_matches_the_printed_summary() {
        let dir = require_fixture(UK_100);
        let inputs = load(&dir).expect("load uk-100 fixture");
        let expires_at = inputs
            .candles
            .last()
            .map(|c| c.time)
            .unwrap_or_else(Utc::now)
            + chrono::Duration::days(365);
        let mark_cfg = DetectorMarkConfig::new(
            DirectionFilter::None,
            GoldenFilter::None,
            inputs.plan.direction,
        );
        let replay = super::super::replay::run(
            &inputs.plan,
            &inputs.candles,
            inputs.meta.granularity,
            inputs.meta.start,
            expires_at,
            mark_cfg,
            None,
        )
        .await;
        let rendered =
            super::super::report::render(&inputs.plan, &replay, true, false, None, &mark_cfg);
        let snapshot = ReplayOutcome::compute(&replay, true, Some(&rendered.economics));

        let net_r = snapshot
            .outcome
            .as_ref()
            .expect("simulated run has economics")
            .net_r;
        // The report prints it as `Net R: {:+.2}` — find that exact rendering.
        let printed = format!("Net R: {net_r:+.2}");
        assert!(
            rendered.text.contains(&printed),
            "saved net_r {net_r} must match the printed summary; looked for {printed:?} in:\n{}",
            rendered.text
        );
    }

    /// With `--simulate` off nothing is booked, so the snapshot carries no
    /// economics — matching the report, which prints no summary in that mode.
    #[tokio::test]
    async fn unsimulated_run_records_no_economics() {
        let dir = require_fixture(UK_100);
        let inputs = load(&dir).expect("load uk-100 fixture");
        let expires_at = inputs
            .candles
            .last()
            .map(|c| c.time)
            .unwrap_or_else(Utc::now)
            + chrono::Duration::days(365);
        let mark_cfg = DetectorMarkConfig::new(
            DirectionFilter::None,
            GoldenFilter::None,
            inputs.plan.direction,
        );
        let replay = super::super::replay::run(
            &inputs.plan,
            &inputs.candles,
            inputs.meta.granularity,
            inputs.meta.start,
            expires_at,
            mark_cfg,
            None,
        )
        .await;
        let rendered =
            super::super::report::render(&inputs.plan, &replay, false, false, None, &mark_cfg);
        let snapshot = ReplayOutcome::compute(&replay, false, Some(&rendered.economics));
        assert!(
            snapshot.outcome.is_none(),
            "an unsimulated run must record no economics: {:?}",
            snapshot.outcome
        );
        // The report still prints a terminal `Net R:` — as `n/a`, never `+0.00`.
        // That is deliberate (commit aeededb): `Net R:` is what batch drivers
        // scrape, so its ABSENCE must mean "the process died", never "nothing was
        // simulated". A run with `--simulate false` is a successful run with no
        // result, and has to say so rather than look like a crash.
        assert!(
            rendered.text.contains("Net R: n/a"),
            "an unsimulated report must still carry a terminal Net R, as n/a:\n{}",
            rendered.text
        );
        assert!(
            !rendered.text.contains("+0.00"),
            "n/a, never +0.00 — a sweep would average +0.00 in as a real trade:\n{}",
            rendered.text
        );
    }

    fn sample_meta() -> FixtureMeta {
        FixtureMeta {
            instrument: "EUR_USD".into(),
            granularity: Granularity::H1,
            source: CandleSource::TradeNation,
            start: Utc.with_ymd_and_hms(2026, 6, 18, 11, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 6, 18, 23, 0, 0).unwrap(),
            message: None,
            arm: None,
        }
    }

    fn sample_outcome() -> ReplayOutcome {
        ReplayOutcome {
            fires: vec![FireOutcome {
                rule_id: "05-enter".into(),
                action: Action::Enter,
                candle_time: Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap(),
                candle_close: 1.2345,
                suppressed_by: Vec::new(),
            }],
            done: true,
            final_phase: trade_control_engine::Phase::Done,
            warnings: vec!["a warning".into()],
            outcome: Some(ReplayEconomics {
                net_r: 1.0,
                tp_hits: 1,
                legs: vec![super::super::economics::Leg {
                    entry_time: Utc.with_ymd_and_hms(2026, 6, 18, 13, 0, 0).unwrap(),
                    entry_price: 1.2300,
                    stop_loss: 1.2200,
                    take_profit: 1.2400,
                    exit_time: Some(Utc.with_ymd_and_hms(2026, 6, 18, 18, 0, 0).unwrap()),
                    exit_price: Some(1.2400),
                    exit_reason: super::super::economics::ExitReason::TookProfit,
                    r: 1.0,
                }],
                ..ReplayEconomics::new()
            }),
        }
    }

    /// A fixture saved before `outcome` existed (no such key) still loads, with
    /// `outcome: None` — so adding the field didn't invalidate the corpus.
    #[test]
    fn expected_without_outcome_still_loads() {
        let legacy = r#"{
            "fires": [],
            "done": true,
            "final_phase": "done",
            "warnings": []
        }"#;
        let loaded: ReplayOutcome = serde_json::from_str(legacy).unwrap();
        assert!(loaded.outcome.is_none());
    }

    /// A no-economics snapshot omits the key entirely, keeping unsimulated
    /// fixtures byte-identical to their pre-field form.
    #[test]
    fn no_outcome_omits_the_key() {
        let outcome = ReplayOutcome {
            outcome: None,
            ..sample_outcome()
        };
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(
            !json.contains("outcome"),
            "a None outcome must omit the key: {json}"
        );
    }

    /// A fire record carries no fill or economic field — the "one fill path"
    /// invariant, asserted on the serialized form so a re-added field is caught
    /// even if it round-trips fine in memory.
    ///
    /// This replaces `sim_outcome_maps_to_fill_outcome`, which checked the
    /// `SimOutcome` → `FillOutcome` mapping that no longer exists. Its subject
    /// was the second fill path; see the module doc for why that was deleted.
    #[test]
    fn a_fire_record_carries_no_fill_or_economics() {
        let json = serde_json::to_string(&sample_outcome()).expect("serialize");
        let fires = json
            .split("\"fires\":")
            .nth(1)
            .and_then(|s| s.split("\"done\":").next())
            .expect("fires array");
        for banned in ["fill", "entry_price", "exit_price", "net_r", "\"r\":"] {
            assert!(
                !fires.contains(banned),
                "a fire must carry no economics — found {banned:?} in {fires}"
            );
        }
    }

    /// A round-trip through serialized JSON is the equality the harness uses.
    #[test]
    fn outcome_json_round_trips() {
        let outcome = sample_outcome();
        let json = serde_json::to_string_pretty(&outcome).unwrap();
        let back: ReplayOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, back);
    }

    /// The arm block survives a full `meta.json` round-trip, and the grouping key
    /// a batch tool needs comes back intact. This is the 4.2 deliverable: six
    /// variants of one trade are now distinguishable **from data**, not filenames.
    #[test]
    fn meta_arm_block_round_trips_with_its_cell_key() {
        use super::super::arm_record::{ArmRecord, EntryRule};
        let meta = FixtureMeta {
            arm: Some(ArmRecord {
                entry_rule: EntryRule::SkipBcr,
                skip_calendar_bars: true,
                skip_golden: false,
                start: Some("2026-07-17T17:00:00+10:00".into()),
                candle_source: Some("tradenation".into()),
                chart_symbol: Some("TRADENATION:EURUSD".into()),
                tv_arm_version: Some("v116-1-gabc".into()),
                engine_version: Some("v116-1-gabc".into()),
                journal_ref: Some("trade-124".into()),
            }),
            ..sample_meta()
        };
        let back: FixtureMeta =
            serde_json::from_str(&serde_json::to_string_pretty(&meta).unwrap()).unwrap();
        assert_eq!(meta, back);
        let arm = back.arm.expect("arm block present");
        assert_eq!(arm.cell_key(), "skip-bcr/news-off");
        // The qualified chart symbol is what makes a wrong-feed capture findable.
        assert_eq!(arm.chart_symbol.as_deref(), Some("TRADENATION:EURUSD"));
        // engine_version is what flags numbers that predate an engine fix.
        assert!(arm.engine_version.is_some());
    }

    /// A fixture saved before the `arm` field existed still loads (as `None`), so
    /// adding it didn't invalidate the corpus.
    #[test]
    fn meta_without_arm_still_loads() {
        let legacy = r#"{
            "instrument": "EUR_USD",
            "granularity": "h1",
            "source": "tradenation",
            "start": "2026-06-18T11:00:00Z",
            "end": "2026-06-18T23:00:00Z"
        }"#;
        let loaded: FixtureMeta = serde_json::from_str(legacy).unwrap();
        assert!(loaded.arm.is_none());
        // And a no-arm meta omits the key entirely.
        let json = serde_json::to_string(&sample_meta()).unwrap();
        assert!(
            !json.contains("arm"),
            "no-arm meta must omit the key: {json}"
        );
    }

    fn sample_plan() -> TradePlan {
        serde_json::from_str(
            r#"{
                "trade_id": "rt-1",
                "instrument": "EUR_USD",
                "direction": "long",
                "granularity": "h1",
                "pip_size": 0.0001,
                "rules": []
            }"#,
        )
        .expect("sample plan parses")
    }

    fn sample_candle() -> EngineCandle {
        EngineCandle {
            time: Utc.with_ymd_and_hms(2026, 6, 18, 11, 0, 0).unwrap(),
            o: 1.0,
            h: 1.5,
            l: 0.9,
            c: 1.2,
            bid_o: 1.0,
            bid_h: 1.5,
            bid_l: 0.9,
            bid_c: 1.2,
            ask_o: 1.0,
            ask_h: 1.5,
            ask_l: 0.9,
            ask_c: 1.2,
        }
    }

    /// `save` then `load` reproduces the inputs; `load_expected` reproduces the
    /// snapshot. Uses a unique temp dir so the test is hermetic.
    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("replay-fixture-test-{}", std::process::id()));
        let plan = sample_plan();
        let candles = vec![sample_candle()];
        let meta = sample_meta();
        let expected = sample_outcome();

        save(&dir, &plan, &candles, &meta, &expected, &[]).unwrap();
        let inputs = load(&dir).unwrap();
        let loaded_expected = load_expected(&dir).unwrap();

        assert_eq!(inputs.candles, candles);
        assert_eq!(inputs.meta, meta);
        // TradePlan has no PartialEq; compare via serialized JSON.
        assert_eq!(
            serde_json::to_value(&inputs.plan).unwrap(),
            serde_json::to_value(&plan).unwrap()
        );
        assert_eq!(loaded_expected, expected);
        // No ambiguous bar ⇒ no sub-bars stored, and NO file written: a fixture
        // saved today is byte-identical on disk to one saved before sub-bar
        // support existed.
        assert!(inputs.sub_bars.is_empty());
        assert!(!dir.join(SUB_BARS_FILE).exists());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sub_bars_round_trip_when_the_zoom_consulted_some() {
        let dir = std::env::temp_dir().join(format!(
            "fixture-subbars-{}",
            std::process::id() as u64 + 991_001
        ));
        fs::remove_dir_all(&dir).ok();
        let plan = sample_plan();
        let candles = vec![sample_candle()];
        let subs = vec![sample_candle()];

        save(
            &dir,
            &plan,
            &candles,
            &sample_meta(),
            &sample_outcome(),
            &subs,
        )
        .unwrap();
        assert!(
            dir.join(SUB_BARS_FILE).exists(),
            "sub_bars.json not written"
        );
        assert_eq!(load(&dir).unwrap().sub_bars, subs);

        // Re-saving with none must REMOVE the stale file, not leave bars behind
        // for a window this run never asks about.
        save(
            &dir,
            &plan,
            &candles,
            &sample_meta(),
            &sample_outcome(),
            &[],
        )
        .unwrap();
        assert!(
            !dir.join(SUB_BARS_FILE).exists(),
            "stale sub_bars.json survived a re-save with no sub-bars"
        );
        assert!(load(&dir).unwrap().sub_bars.is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    /// A fixture directory with no `sub_bars.json` — every fixture saved before
    /// this feature — must load as "none stored" rather than erroring.
    #[test]
    fn a_fixture_without_sub_bars_json_still_loads() {
        let dir = std::env::temp_dir().join(format!(
            "fixture-nosubbars-{}",
            std::process::id() as u64 + 991_002
        ));
        fs::remove_dir_all(&dir).ok();
        save(
            &dir,
            &sample_plan(),
            &[sample_candle()],
            &sample_meta(),
            &sample_outcome(),
            &[],
        )
        .unwrap();
        assert!(!dir.join(SUB_BARS_FILE).exists());
        assert!(
            load(&dir)
                .expect("loads without sub_bars.json")
                .sub_bars
                .is_empty()
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// A `--message` note survives a `save` → `load` round-trip in the meta.
    #[test]
    fn meta_message_round_trips() {
        let meta = FixtureMeta {
            message: Some("pins the UK100 fall-into-support reversal-close (v112)".into()),
            ..sample_meta()
        };
        let json = serde_json::to_string_pretty(&meta).unwrap();
        let back: FixtureMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, back);
        assert_eq!(
            back.message.as_deref(),
            Some("pins the UK100 fall-into-support reversal-close (v112)")
        );
    }

    /// No message → the key is omitted from the JSON, so fixtures saved before
    /// this field existed still deserialize (serde `default`).
    #[test]
    fn meta_without_message_omits_key_and_loads() {
        let meta = sample_meta();
        assert!(meta.message.is_none());
        let json = serde_json::to_string(&meta).unwrap();
        assert!(
            !json.contains("message"),
            "no-message meta must omit the key: {json}"
        );
        // A pre-field fixture (no `message` key at all) still loads.
        let legacy = r#"{
            "instrument": "EUR_USD",
            "granularity": "h1",
            "source": "tradenation",
            "start": "2026-06-18T11:00:00Z",
            "end": "2026-06-18T23:00:00Z"
        }"#;
        let loaded: FixtureMeta = serde_json::from_str(legacy).unwrap();
        assert!(loaded.message.is_none());
    }
}
