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
//! ```
//!
//! The snapshot schema ([`ReplayOutcome`]) is owned here, not in the engine — it
//! captures exactly what the test should assert (each fire's decision, its
//! simulated fill, and the run's economics), independent of `report.rs`'s
//! human-facing text.
//!
//! ## Two fill paths — mind the gap
//!
//! `ReplayOutcome` carries **two** views of what happened, from two different
//! computations. Know which one you're reading:
//!
//! - **`outcome`** ([`ReplayEconomics`]) — net R, counts, and per-position legs,
//!   booked by `report::render` off the `ReplayBroker` held ledger
//!   (`fire.realized`). **This is the authoritative economic result**, and it is
//!   the same value the report prints, passed in rather than recomputed.
//! - **`fires[].fill`** — a per-fire [`FillOutcome`] from [`fill_for`] →
//!   `fill_sim::simulate_fill`, an *independent re-simulation*. It has **no
//!   reversal- or expiry-close awareness**, so it cannot represent either
//!   outcome: a position the ledger flattened on a reversal shows here as
//!   whatever its bracket would have done on its own.
//!
//! So for a reversal/expiry close, `outcome.legs` is right and `fires[].fill`
//! is misleading. `fill` is retained because it pins the *bracket* physics
//! (where a resting order would fill, sweep, or never trigger) independently of
//! the ledger — a genuine second opinion on entry mechanics. Collapsing it onto
//! `fire.realized` too would lose that; if it ever moves, the reversal/expiry
//! blindness is the reason, not a cosmetic cleanup.

use std::fs;
use std::path::Path;

use super::fill_sim::{SimOutcome, simulate_fill};
use chrono::{DateTime, Utc};
use color_eyre::eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use trade_control_core::intent::{Action, Shell};
use trade_control_engine::{BidAskCandle as EngineCandle, Granularity, TradePlan};

use super::arm_record::ArmRecord;
use super::economics::ReplayEconomics;
use super::replay::{Fire, Replay};
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

/// The golden snapshot of a replay: every fire's decision plus its simulated
/// fill, the terminal flag/phase, and the deduped warnings. Equality is by
/// serialized JSON (floats compare exactly as written), which is what the test
/// harness asserts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// One fired intent's decision: which rule, what action, on which bar — plus the
/// simulated fill when it was an enter (and simulation was on).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FireOutcome {
    pub rule_id: String,
    /// The intent's action (serde kebab-case, e.g. `enter` / `veto`).
    pub action: Action,
    /// Open-time of the triggering candle.
    pub candle_time: DateTime<Utc>,
    /// Close of the triggering candle.
    pub candle_close: f64,
    /// The simulated fill, present only for an enter when simulation was on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<FillOutcome>,
    /// Active news-blackout ids that suppressed this enter (paused at fire
    /// time). Empty/omitted for any fire the blackout gate let through. A
    /// suppressed enter has no `fill` — it's a 0R skip. Serialized so a fixture
    /// freezes the with-blackout SKIP as a regression.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suppressed_by: Vec<String>,
}

/// A flat, serializable mirror of [`SimOutcome`]. Mirroring it here (rather than
/// serializing the engine type) keeps the golden value owned by the test harness
/// and decoupled from any cosmetic change to the engine enum's shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FillOutcome {
    NeverFilled,
    FilledOpen {
        fill_at: DateTime<Utc>,
        entry_price: f64,
    },
    StoppedOut {
        fill_at: DateTime<Utc>,
        entry_price: f64,
        exit_at: DateTime<Utc>,
        exit_price: f64,
    },
    TookProfit {
        fill_at: DateTime<Utc>,
        entry_price: f64,
        exit_at: DateTime<Utc>,
        exit_price: f64,
    },
    Unresolved {
        reason: String,
    },
    Declined {
        name: String,
    },
}

impl From<&SimOutcome> for FillOutcome {
    fn from(o: &SimOutcome) -> Self {
        match o {
            SimOutcome::NeverFilled => FillOutcome::NeverFilled,
            SimOutcome::FilledOpen {
                fill_at,
                entry_price,
            } => FillOutcome::FilledOpen {
                fill_at: *fill_at,
                entry_price: *entry_price,
            },
            SimOutcome::StoppedOut {
                fill_at,
                entry_price,
                exit_at,
                exit_price,
            } => FillOutcome::StoppedOut {
                fill_at: *fill_at,
                entry_price: *entry_price,
                exit_at: *exit_at,
                exit_price: *exit_price,
            },
            SimOutcome::TookProfit {
                fill_at,
                entry_price,
                exit_at,
                exit_price,
            } => FillOutcome::TookProfit {
                fill_at: *fill_at,
                entry_price: *entry_price,
                exit_at: *exit_at,
                exit_price: *exit_price,
            },
            SimOutcome::Unresolved(reason) => FillOutcome::Unresolved {
                reason: reason.clone(),
            },
            SimOutcome::Declined { name } => FillOutcome::Declined { name: name.clone() },
        }
    }
}

/// Simulate one fire's fill, the single source both the report and the snapshot
/// use. Returns `None` for a non-enter fire or when `simulate` is off — exactly
/// the cases the report shows no fill for. Reconstructs the dispatch `Shell`
/// from the fire (folding the latched H&S signal when present) so the simulator
/// resolves the same entry/SL/TP levels the live worker would.
pub fn fill_for(plan: &TradePlan, fire: &Fire, simulate: bool) -> Option<SimOutcome> {
    if !simulate || fire.fired.intent.action != Action::Enter {
        return None;
    }
    // An enter the real `run_enter` rejected (paused, cooled-down, vetoed, …)
    // never placed an order, so its standalone fill is fiction — no fill, exactly
    // like a superseded one.
    if fire.rejected_reason().is_some() {
        return None;
    }
    let candle = &fire.fired.candle;
    let shell = match &fire.fired.signal {
        Some(sig) => Shell::from_candle_and_signal(candle, sig),
        None => Shell::from_candle(candle),
    };
    Some(simulate_fill(
        &fire.fired.intent,
        &shell,
        plan.pip_size,
        &fire.forward,
    ))
}

impl ReplayOutcome {
    /// Build the golden snapshot from a completed [`Replay`]. `simulate` mirrors
    /// the run's `--simulate` flag (off → fills are `None`, matching the report).
    ///
    /// `economics` is what `report::render` booked for this same replay. It is
    /// **passed in rather than recomputed** on purpose: booking it twice is how
    /// the report and the golden diverged in the first place (see the module
    /// doc). Pass `None` for a non-simulated run.
    pub fn compute(
        plan: &TradePlan,
        replay: &Replay,
        simulate: bool,
        economics: Option<&ReplayEconomics>,
    ) -> Self {
        let fires = replay
            .fires
            .iter()
            .map(|fire| FireOutcome {
                rule_id: fire.fired.rule_id.clone(),
                action: fire.fired.intent.action,
                candle_time: fire.fired.candle.time,
                candle_close: fire.fired.candle.c,
                fill: fill_for(plan, fire, simulate).map(|o| (&o).into()),
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

/// Write a complete fixture to `dir` (created if absent): the plan, the frozen
/// candle window, the resolved meta, and the expected outcome — each as
/// pretty-printed JSON for readable diffs.
pub fn save(
    dir: &Path,
    plan: &TradePlan,
    candles: &[EngineCandle],
    meta: &FixtureMeta,
    expected: &ReplayOutcome,
) -> Result<()> {
    fs::create_dir_all(dir).wrap_err_with(|| format!("create fixture dir {}", dir.display()))?;
    write_json(&dir.join(PLAN_FILE), plan)?;
    write_json(&dir.join(CANDLES_FILE), &candles.to_vec())?;
    write_json(&dir.join(META_FILE), meta)?;
    write_json(&dir.join(EXPECTED_FILE), expected)?;
    Ok(())
}

/// The frozen inputs of a fixture, loaded back for an offline replay.
pub struct FixtureInputs {
    pub plan: TradePlan,
    pub candles: Vec<EngineCandle>,
    pub meta: FixtureMeta,
}

/// Read a fixture's inputs (plan + candles + meta) from `dir`. The expected
/// outcome is read separately by the caller that needs it ([`load_expected`]).
pub fn load(dir: &Path) -> Result<FixtureInputs> {
    Ok(FixtureInputs {
        plan: read_json(&dir.join(PLAN_FILE))?,
        candles: read_json(&dir.join(CANDLES_FILE))?,
        meta: read_json(&dir.join(META_FILE))?,
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
    fs::write(path, json).wrap_err_with(|| format!("write {}", path.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).wrap_err_with(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).wrap_err_with(|| format!("parse {}", path.display()))
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
    use super::*;
    use chrono::TimeZone;
    use std::path::PathBuf;
    use trade_control_cli::replay_args::{DetectorMarkConfig, DirectionFilter, GoldenFilter};

    /// List the fixture directories under `replay-fixtures/` (each holding the
    /// four JSON files), sorted for deterministic test ordering. Empty when the
    /// dir is absent or has no sub-dirs — the harness then no-ops.
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
    /// verified verdict fails here. No-ops (with a note) until fixtures exist.
    #[tokio::test]
    async fn all_fixtures_match_expected() {
        let dirs = fixture_dirs();
        if dirs.is_empty() {
            eprintln!(
                "no fixtures under {} — save one with `replay-candles ... --save <name>`",
                super::fixtures_root().display()
            );
            return;
        }
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
                // No finer series for a frozen fixture (only its saved coarse
                // candles), so the sub-bar zoom is inactive → pessimistic stop on
                // an ambiguous SL/TP bar, exactly as the saved outcome expects.
                &[],
            )
            .await;
            // Fixtures are saved from `--simulate` runs (the default), so the
            // golden outcome carries fills AND economics; recompute both the same
            // way the binary does — render (which books) then snapshot.
            let rendered =
                super::super::report::render(&inputs.plan, &replay, true, false, None, &mark_cfg);
            let computed =
                ReplayOutcome::compute(&inputs.plan, &replay, true, Some(&rendered.economics));

            assert_eq!(
                computed,
                expected,
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
    async fn stateful_broker_books_reversal_and_expiry_closes_in_the_report() {
        let dir = super::fixtures_root().join("uk-100-news-blackout-rentry-close-on-reversal");
        if !dir.is_dir() {
            eprintln!("uk-100 reversal/expiry fixture missing — skipping");
            return;
        }
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
            &[],
        )
        .await;
        let rendered =
            super::super::report::render(&inputs.plan, &replay, true, false, None, &mark_cfg);
        let econ = &rendered.economics;

        // A reversal-close must book a real position off the held ledger, not sit
        // at 0R / "still open". Before S5 `close_positions` was a no-op.
        assert_eq!(
            econ.reversal_closes, 1,
            "reversal-close must be booked from the held ledger: {econ:#?}"
        );
        // The trade-expiry ClosePositions veto must FLATTEN the open position at
        // the expiry candle's close — the operator's original ask. Before S5 the
        // veto left the position open at 0R.
        assert_eq!(
            econ.expiry_closes, 1,
            "expiry veto must flatten the open position from the held ledger: {econ:#?}"
        );

        // Both closes are booked as legs with a real exit and a signed R — the
        // half the old `simulate_fill` golden could not represent at all. This is
        // the assertion that now guards Net R against a silent regression.
        let closed: Vec<_> = econ
            .legs
            .iter()
            .filter(|l| {
                matches!(
                    l.exit_reason,
                    super::super::economics::ExitReason::Reversal
                        | super::super::economics::ExitReason::Expiry
                )
            })
            .collect();
        assert_eq!(closed.len(), 2, "both closes must book legs: {econ:#?}");
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

        // The rendered text must still SHOW them (formatting regression guard).
        let text = &rendered.text;
        for want in ["CLOSED ON REVERSAL", "CLOSED AT EXPIRY", "EXP: 1"] {
            assert!(text.contains(want), "report must show {want}:\n{text}");
        }

        // And the SNAPSHOT must carry the same economics — this is the half that
        // `--check` gates on, and the reason a silent Net R regression can no
        // longer pass while firing identical rules.
        let snapshot = ReplayOutcome::compute(&inputs.plan, &replay, true, Some(econ));
        assert_eq!(
            snapshot.outcome.as_ref(),
            Some(econ),
            "the golden snapshot must record the booked economics verbatim"
        );
    }

    /// The printed `Net R:` line and the saved `outcome.net_r` are the same
    /// number, on a real fixture. They were two computations before
    /// `ReplayEconomics`; this pins them together.
    #[tokio::test]
    async fn saved_net_r_matches_the_printed_summary() {
        let dir = super::fixtures_root().join("uk-100-news-blackout-rentry-close-on-reversal");
        if !dir.is_dir() {
            eprintln!("uk-100 fixture missing — skipping");
            return;
        }
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
            &[],
        )
        .await;
        let rendered =
            super::super::report::render(&inputs.plan, &replay, true, false, None, &mark_cfg);
        let snapshot =
            ReplayOutcome::compute(&inputs.plan, &replay, true, Some(&rendered.economics));

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
        let dir = super::fixtures_root().join("uk-100-news-blackout-rentry-close-on-reversal");
        if !dir.is_dir() {
            eprintln!("uk-100 fixture missing — skipping");
            return;
        }
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
            &[],
        )
        .await;
        let rendered =
            super::super::report::render(&inputs.plan, &replay, false, false, None, &mark_cfg);
        let snapshot =
            ReplayOutcome::compute(&inputs.plan, &replay, false, Some(&rendered.economics));
        assert!(
            snapshot.outcome.is_none(),
            "an unsimulated run must record no economics: {:?}",
            snapshot.outcome
        );
        assert!(
            !rendered.text.contains("Net R:"),
            "an unsimulated report must print no summary:\n{}",
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
                fill: Some(FillOutcome::TookProfit {
                    fill_at: Utc.with_ymd_and_hms(2026, 6, 18, 13, 0, 0).unwrap(),
                    entry_price: 1.2300,
                    exit_at: Utc.with_ymd_and_hms(2026, 6, 18, 18, 0, 0).unwrap(),
                    exit_price: 1.2400,
                }),
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

    /// Every `SimOutcome` variant maps to its `FillOutcome` twin (the report and
    /// the snapshot rely on this being total).
    #[test]
    fn sim_outcome_maps_to_fill_outcome() {
        let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let cases = [
            (SimOutcome::NeverFilled, FillOutcome::NeverFilled),
            (
                SimOutcome::FilledOpen {
                    fill_at: at,
                    entry_price: 1.0,
                },
                FillOutcome::FilledOpen {
                    fill_at: at,
                    entry_price: 1.0,
                },
            ),
            (
                SimOutcome::Declined {
                    name: "too-low".into(),
                },
                FillOutcome::Declined {
                    name: "too-low".into(),
                },
            ),
            (
                SimOutcome::Unresolved("bad geometry".into()),
                FillOutcome::Unresolved {
                    reason: "bad geometry".into(),
                },
            ),
        ];
        for (sim, want) in cases {
            assert_eq!(FillOutcome::from(&sim), want);
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
                broker: Some("tradenation".into()),
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

    /// `save` then `load` reproduces the inputs; `load_expected` reproduces the
    /// snapshot. Uses a unique temp dir so the test is hermetic.
    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("replay-fixture-test-{}", std::process::id()));
        let plan: TradePlan = serde_json::from_str(
            r#"{
                "trade_id": "rt-1",
                "instrument": "EUR_USD",
                "direction": "long",
                "granularity": "h1",
                "pip_size": 0.0001,
                "rules": []
            }"#,
        )
        .unwrap();
        let candles = vec![EngineCandle {
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
        }];
        let meta = sample_meta();
        let expected = sample_outcome();

        save(&dir, &plan, &candles, &meta, &expected).unwrap();
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
