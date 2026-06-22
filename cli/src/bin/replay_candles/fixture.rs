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
//! expected.json  — the golden ReplayOutcome snapshot
//! ```
//!
//! The snapshot schema ([`ReplayOutcome`]) is owned here, not in the engine — it
//! captures exactly what the test should assert (each fire's decision and its
//! simulated fill), independent of `report.rs`'s human-facing text. Both the
//! report and the snapshot compute their fill via the single [`fill_for`] path,
//! so they can't diverge.

use std::fs;
use std::path::Path;

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use trade_control_core::intent::{Action, Shell};
use trade_control_engine::{
    Candle as EngineCandle, Granularity, SimOutcome, TradePlan, simulate_fill,
};

use super::replay::{Fire, Replay};
use super::source::CandleSource;

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
    pub fn compute(plan: &TradePlan, replay: &Replay, simulate: bool) -> Self {
        let fires = replay
            .fires
            .iter()
            .map(|fire| FireOutcome {
                rule_id: fire.fired.rule_id.clone(),
                action: fire.fired.intent.action,
                candle_time: fire.fired.candle.time,
                candle_close: fire.fired.candle.c,
                fill: fill_for(plan, fire, simulate).map(|o| (&o).into()),
            })
            .collect();
        ReplayOutcome {
            fires,
            done: replay.done,
            final_phase: replay.final_state.phase,
            warnings: replay.warnings.clone(),
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
    #[test]
    fn all_fixtures_match_expected() {
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
            let replay = super::super::replay::run(
                &inputs.plan,
                &inputs.candles,
                inputs.meta.granularity,
                expires_at,
            );
            // Fixtures are saved from `--simulate` runs (the default), so the
            // golden outcome carries fills; recompute with simulation on.
            let computed = ReplayOutcome::compute(&inputs.plan, &replay, true);

            assert_eq!(
                computed,
                expected,
                "fixture {name} diverged:\n got: {}\n exp: {}",
                serde_json::to_string_pretty(&computed).unwrap_or_default(),
                serde_json::to_string_pretty(&expected).unwrap_or_default(),
            );
        }
    }

    fn sample_meta() -> FixtureMeta {
        FixtureMeta {
            instrument: "EUR_USD".into(),
            granularity: Granularity::H1,
            source: CandleSource::TradeNation,
            start: Utc.with_ymd_and_hms(2026, 6, 18, 11, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 6, 18, 23, 0, 0).unwrap(),
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
            }],
            done: true,
            final_phase: trade_control_engine::Phase::Done,
            warnings: vec!["a warning".into()],
        }
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
}
