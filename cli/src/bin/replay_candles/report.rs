//! Format the replay outcome: each fire, and — for enters — the simulated fill.
//!
//! The shell-from-fire reconstruction mirrors the worker's `dispatch_fired`
//! (an H&S Pine fire folds its latched signal onto the shell; everything else
//! gets the plain candle shell), so `simulate_fill` resolves entry/SL/TP against
//! the same levels the live dispatch would have.

use trade_control_core::intent::{Action, Shell};
use trade_control_engine::{SimOutcome, TradePlan, simulate_fill};

use super::replay::{Fire, Replay};

/// Render the full replay report as a string.
pub fn render(plan: &TradePlan, replay: &Replay, simulate: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Plan {} ({}, {:?}) — {} fire(s) over the window\n",
        plan.trade_id,
        plan.instrument,
        plan.granularity,
        replay.fires.len()
    ));

    let mut wins = 0usize;
    let mut losses = 0usize;
    for fire in &replay.fires {
        out.push_str(&render_fire(plan, fire, simulate, &mut wins, &mut losses));
    }

    if !replay.warnings.is_empty() {
        out.push_str("\nWarnings:\n");
        for w in &replay.warnings {
            out.push_str(&format!("  - {w}\n"));
        }
    }

    out.push_str(&format!(
        "\nDone: {}  |  final phase: {:?}  |  fires: {}",
        replay.done,
        replay.final_state.phase,
        replay.fires.len()
    ));
    if simulate {
        out.push_str(&format!("  |  TP: {wins}  SL: {losses}"));
    }
    out.push('\n');
    out
}

fn render_fire(
    plan: &TradePlan,
    fire: &Fire,
    simulate: bool,
    wins: &mut usize,
    losses: &mut usize,
) -> String {
    let intent = &fire.fired.intent;
    let candle = &fire.fired.candle;
    let mut line = format!(
        "\n• {} {:?} @ {}  close={}\n",
        fire.fired.rule_id, intent.action, candle.time, candle.c
    );

    if !simulate {
        return line;
    }
    if intent.action != Action::Enter {
        line.push_str("    (non-enter fire — no fill simulated)\n");
        return line;
    }

    // Reconstruct the shell exactly as the worker's dispatch would, so the
    // simulator resolves the same entry/SL/TP levels.
    let shell = match &fire.fired.signal {
        Some(sig) => Shell::from_candle_and_signal(candle, sig),
        None => Shell::from_candle(candle),
    };
    let outcome = simulate_fill(intent, &shell, plan.pip_size, &fire.forward);
    line.push_str(&format!("    {}\n", describe_outcome(&outcome)));
    match outcome {
        SimOutcome::TookProfit { .. } => *wins += 1,
        SimOutcome::StoppedOut { .. } => *losses += 1,
        _ => {}
    }
    line
}

fn describe_outcome(outcome: &SimOutcome) -> String {
    match outcome {
        SimOutcome::NeverFilled => {
            "fill: NEVER FILLED (pending order untriggered in window)".into()
        }
        SimOutcome::FilledOpen {
            fill_at,
            entry_price,
        } => format!("fill: FILLED @ {entry_price} ({fill_at}) — still open at window end"),
        SimOutcome::StoppedOut {
            fill_at,
            entry_price,
            exit_at,
            exit_price,
        } => format!(
            "fill: STOPPED OUT — in @ {entry_price} ({fill_at}) → SL {exit_price} ({exit_at})"
        ),
        SimOutcome::TookProfit {
            fill_at,
            entry_price,
            exit_at,
            exit_price,
        } => format!(
            "fill: TOOK PROFIT — in @ {entry_price} ({fill_at}) → TP {exit_price} ({exit_at})"
        ),
        SimOutcome::Unresolved(reason) => format!("fill: UNRESOLVED — {reason}"),
    }
}
