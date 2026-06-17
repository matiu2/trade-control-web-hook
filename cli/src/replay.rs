//! `replay` — re-run a recorded engine tick-bundle through the pure evaluator
//! and diff the result against what was recorded.
//!
//! A [`TickBundle`](trade_control_core::tick_bundle::TickBundle) captures the
//! full input tuple `evaluate_plan` consumed on one cron tick, plus its golden
//! [`PlanEval`] output. This command reads one bundle (a local JSON file written
//! under the worker's `ticks/` R2 prefix, or pulled from R2), re-runs the *same*
//! pure `evaluate_plan` on the recorded inputs, and diffs the fresh `fired` /
//! `new_state` / `done` against the recorded ones.
//!
//! A zero-diff replay is evidence the decision logic is unchanged; a diff after
//! a code change is exactly the regression signal we want — fix a bug, replay
//! the bundle that exhibited it, watch the outcome change. Exit code is non-zero
//! on any mismatch so it gates in CI.
//!
//! This is the *pure-evaluator* replay (the roadmap's fast inner-loop path). It
//! validates the decision logic, not the worker glue or broker dispatch — the
//! recorded `dispatch_outcomes` are shown for context but a broker-simulator
//! replay of them is a later step.

use std::path::{Path, PathBuf};

use clap::Parser;
use color_eyre::eyre::{Context, Result, eyre};
use trade_control_core::tick_bundle::TickBundle;
use trade_control_engine::evaluate_plan;

/// Args for the `replay` subcommand.
#[derive(Parser)]
pub struct ReplayArgs {
    /// Path to a recorded tick-bundle JSON file (a `ticks/.../<...>.json`
    /// object, fetched from R2 or a local fixture).
    pub bundle: PathBuf,
}

/// Load a tick-bundle from a JSON file.
pub fn load_bundle_from_file(path: &Path) -> Result<TickBundle> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading bundle {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("parsing bundle {} as JSON", path.display()))
}

/// The outcome of a replay: whether the re-run matched, and a human diff.
pub struct ReplayReport {
    pub matched: bool,
    pub lines: Vec<String>,
}

/// Re-run `evaluate_plan` on the bundle's recorded inputs and diff the fresh
/// result against the recorded `eval`. Equality is by serialized JSON (the
/// `PlanEval` graph carries `Intent`, which has no `PartialEq`).
pub fn replay_bundle(bundle: &TickBundle) -> Result<ReplayReport> {
    let fresh = evaluate_plan(
        &bundle.plan,
        &bundle.prior_state,
        &bundle.new_candles,
        &bundle.detector_window,
        bundle.now,
        bundle.expires_at,
    );

    let recorded_fired = serde_json::to_value(&bundle.eval.fired).context("recorded fired")?;
    let fresh_fired = serde_json::to_value(&fresh.fired).context("replayed fired")?;
    let recorded_state = serde_json::to_value(&bundle.eval.new_state).context("recorded state")?;
    let fresh_state = serde_json::to_value(&fresh.new_state).context("replayed state")?;

    let fired_match = recorded_fired == fresh_fired;
    let state_match = recorded_state == fresh_state;
    let done_match = bundle.eval.done == fresh.done;
    let matched = fired_match && state_match && done_match;

    let mut lines = Vec::new();
    lines.push(format!(
        "replay {} (tick {})",
        bundle.correlation_id, bundle.request_id
    ));
    lines.push(diff_line(
        "fired",
        fired_match,
        &format!("{} intent(s)", bundle.eval.fired.len()),
        &format!("{} intent(s)", fresh.fired.len()),
    ));
    lines.push(diff_line(
        "new_state",
        state_match,
        &format!("phase={:?}", bundle.eval.new_state.phase),
        &format!("phase={:?}", fresh.new_state.phase),
    ));
    lines.push(diff_line(
        "done",
        done_match,
        &bundle.eval.done.to_string(),
        &fresh.done.to_string(),
    ));

    Ok(ReplayReport { matched, lines })
}

/// One per-field diff line: a tick when it matched, a cross with both sides when
/// it didn't.
fn diff_line(field: &str, matched: bool, recorded: &str, replayed: &str) -> String {
    if matched {
        format!("  ✓ {field}: {recorded}")
    } else {
        format!("  ✗ {field}: recorded {recorded} != replayed {replayed}")
    }
}

/// Run the `replay` subcommand: load, replay, print the report, and signal a
/// mismatch as an error (non-zero exit) so it gates in CI.
pub fn run_replay(args: ReplayArgs) -> Result<()> {
    let bundle = load_bundle_from_file(&args.bundle)?;
    let report = replay_bundle(&bundle)?;
    for line in &report.lines {
        println!("{line}");
    }
    if !bundle.dispatch_outcomes.is_empty() {
        println!("  recorded dispatch outcomes (not replayed here):");
        for o in &bundle.dispatch_outcomes {
            println!("    [{}] {} → {}", o.seq, o.rule_id, o.outcome);
        }
    }
    if report.matched {
        println!("MATCH — the pure evaluation is unchanged.");
        Ok(())
    } else {
        Err(eyre!(
            "MISMATCH — the replay diverged from the recorded tick"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bundle whose recorded `eval` is what `evaluate_plan` produces for a
    /// single M/W-style enter rule firing on one fresh candle. The plan has no
    /// preps, so it starts in `await_entry`; an `mw_every_bar` enter fires once
    /// per closed bar. Recorded `eval` below is the genuine output for this
    /// input — so a faithful replay matches.
    fn faithful_bundle_json() -> String {
        r#"{
          "schema_version": 1,
          "tick_ts": "2026-06-17T20:00:00Z",
          "correlation_id": "mw-eurusd-test",
          "account": null,
          "request_id": "mw-eurusd-test@2026-06-17T20:00:00Z",
          "plan": {
            "trade_id": "mw-eurusd-test",
            "instrument": "EUR_USD",
            "direction": "long",
            "granularity": "h1",
            "pip_size": 0.0001,
            "rules": [
              {
                "rule_id": "05-enter",
                "trigger": { "type": "mw_every_bar" },
                "fire_mode": "every_bar",
                "intent": {
                  "v": 1,
                  "id": "mw-eurusd-test-enter",
                  "not_after": "2026-06-21T00:00:00Z",
                  "action": "enter",
                  "instrument": "EUR_USD",
                  "direction": "long",
                  "broker": "oanda",
                  "trade_id": "mw-eurusd-test"
                }
              }
            ],
            "shadow": false
          },
          "prior_state": {
            "watermark": "2026-06-17T18:00:00Z",
            "phase": "await_entry",
            "expires_at": "2026-06-18T20:00:00Z"
          },
          "new_candles": [
            { "time": "2026-06-17T19:00:00Z", "o": 1.1, "h": 1.11, "l": 1.09, "c": 1.105 }
          ],
          "detector_window": [
            { "time": "2026-06-17T19:00:00Z", "o": 1.1, "h": 1.11, "l": 1.09, "c": 1.105 }
          ],
          "now": "2026-06-17T20:00:00Z",
          "expires_at": "2026-06-18T20:00:00Z",
          "eval": PLACEHOLDER_EVAL,
          "shadow": false,
          "dispatch_outcomes": [],
          "kv": {
            "key": "plan-state:<global>:mw-eurusd-test",
            "before": null,
            "after": null,
            "cleared_plan": false,
            "success": true,
            "error": null
          }
        }"#
        .to_string()
    }

    /// Build a faithful bundle by computing the *real* `eval` for the fixture
    /// inputs and splicing it in — so the test can't drift from the evaluator.
    fn faithful_bundle() -> TickBundle {
        // Parse a bundle with a throwaway eval, recompute the true eval, splice.
        let stub = faithful_bundle_json().replace(
            "PLACEHOLDER_EVAL",
            r#"{"fired":[],"new_state":{"phase":"await_entry","expires_at":"2026-06-18T20:00:00Z"},"done":false}"#,
        );
        let mut b: TickBundle = serde_json::from_str(&stub).expect("stub parses");
        let real = evaluate_plan(
            &b.plan,
            &b.prior_state,
            &b.new_candles,
            &b.detector_window,
            b.now,
            b.expires_at,
        );
        b.eval = real;
        b
    }

    #[test]
    fn faithful_bundle_replays_as_match() {
        let bundle = faithful_bundle();
        let report = replay_bundle(&bundle).expect("replay runs");
        assert!(
            report.matched,
            "a bundle recording the evaluator's own output must replay as MATCH; diff:\n{}",
            report.lines.join("\n")
        );
    }

    #[test]
    fn tampered_done_flag_replays_as_mismatch() {
        let mut bundle = faithful_bundle();
        // Flip the recorded `done` so it disagrees with a fresh evaluation.
        bundle.eval.done = !bundle.eval.done;
        let report = replay_bundle(&bundle).expect("replay runs");
        assert!(
            !report.matched,
            "a bundle with a tampered recorded output must replay as MISMATCH"
        );
    }
}
