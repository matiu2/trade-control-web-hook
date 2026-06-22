//! Format the replay outcome: each fire, and — for enters — the simulated fill.
//!
//! The shell-from-fire reconstruction mirrors the worker's `dispatch_fired`
//! (an H&S Pine fire folds its latched signal onto the shell; everything else
//! gets the plain candle shell), so `simulate_fill` resolves entry/SL/TP against
//! the same levels the live dispatch would have.

use trade_control_core::intent::{Action, Direction, Resolved, ResolvedEntry, Shell};
use trade_control_engine::{SimOutcome, TradePlan, simulate_fill};

use super::brisbane::bne;
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
        fire.fired.rule_id,
        intent.action,
        bne(candle.time),
        candle.c
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

    // Resolve the bracket the worker would have placed (direction, entry, SL,
    // TP) and print it for every enter — even ones the price path never fills.
    // This is the journaling line: what order went on at this bar.
    match Resolved::from_intent(intent, &shell, plan.pip_size) {
        Ok(resolved) => line.push_str(&format!(
            "    {}\n",
            describe_order(&resolved, plan.pip_size)
        )),
        Err(err) => line.push_str(&format!("    order: UNRESOLVED — {err:?}\n")),
    }

    let outcome = simulate_fill(intent, &shell, plan.pip_size, &fire.forward);
    line.push_str(&format!("    {}\n", describe_outcome(&outcome)));
    match outcome {
        SimOutcome::TookProfit { .. } => *wins += 1,
        SimOutcome::StoppedOut { .. } => *losses += 1,
        _ => {}
    }
    line
}

/// One-line summary of the bracket order the worker would have placed: the
/// direction, the order type + entry level, and the SL / TP. Printed for every
/// enter fire so the report doubles as a journaling record — including orders
/// that the price path never fills. Prices are rounded to the instrument's
/// price grid (`pip_size`) so resolved levels read cleanly instead of carrying
/// binary-float noise (e.g. `0.46367`, not `0.46366999999999997`).
fn describe_order(resolved: &Resolved, pip_size: f64) -> String {
    let dir = match resolved.direction {
        Direction::Long => "LONG",
        Direction::Short => "SHORT",
    };
    let entry = match resolved.entry {
        ResolvedEntry::Market { reference_price } => {
            format!("market @ {}", fmt_price(reference_price, pip_size))
        }
        ResolvedEntry::Stop { trigger_price } => {
            format!("stop @ {}", fmt_price(trigger_price, pip_size))
        }
        ResolvedEntry::Limit { trigger_price } => {
            format!("limit @ {}", fmt_price(trigger_price, pip_size))
        }
    };
    format!(
        "order: {dir} {entry}  SL {}  TP {}",
        fmt_price(resolved.stop_loss, pip_size),
        fmt_price(resolved.take_profit, pip_size)
    )
}

/// Format a price at the instrument's display precision: one decimal place
/// finer than `pip_size` (a fractional-pip digit), which covers FX 5/3-dp
/// quotes and index whole-point grids alike. Falls back to 5 decimals when
/// `pip_size` is non-positive.
fn fmt_price(price: f64, pip_size: f64) -> String {
    let decimals = if pip_size > 0.0 {
        // pip_size 0.0001 → 4 pip decimals → 5 shown; 1.0 → 0 → 1 shown.
        (-pip_size.log10()).round().max(0.0) as usize + 1
    } else {
        5
    };
    format!("{price:.decimals$}")
}

fn describe_outcome(outcome: &SimOutcome) -> String {
    match outcome {
        SimOutcome::NeverFilled => {
            "fill: NEVER FILLED (pending order untriggered in window)".into()
        }
        SimOutcome::FilledOpen {
            fill_at,
            entry_price,
        } => format!(
            "fill: FILLED @ {entry_price} ({}) — still open at window end",
            bne(*fill_at)
        ),
        SimOutcome::StoppedOut {
            fill_at,
            entry_price,
            exit_at,
            exit_price,
        } => format!(
            "fill: STOPPED OUT — in @ {entry_price} ({}) → SL {exit_price} ({})",
            bne(*fill_at),
            bne(*exit_at)
        ),
        SimOutcome::TookProfit {
            fill_at,
            entry_price,
            exit_at,
            exit_price,
        } => format!(
            "fill: TOOK PROFIT — in @ {entry_price} ({}) → TP {exit_price} ({})",
            bne(*fill_at),
            bne(*exit_at)
        ),
        SimOutcome::Unresolved(reason) => format!("fill: UNRESOLVED — {reason}"),
        SimOutcome::Declined { name } => {
            format!("fill: DECLINED — entry past the {name} level (no order placed)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use trade_control_core::intent::RiskBudget;

    fn resolved(direction: Direction, entry: ResolvedEntry, sl: f64, tp: f64) -> Resolved {
        Resolved {
            id: "r".into(),
            not_after: Utc.with_ymd_and_hms(2026, 6, 30, 0, 0, 0).unwrap(),
            instrument: "NZD_CHF".into(),
            direction,
            entry,
            stop_loss: sl,
            take_profit: tp,
            risk: RiskBudget::Percent(1.0),
            dry_run: false,
            on_too_close: None,
        }
    }

    #[test]
    fn short_stop_order_shows_direction_entry_and_levels() {
        let r = resolved(
            Direction::Short,
            ResolvedEntry::Stop {
                trigger_price: 0.46311,
            },
            0.46338,
            0.46221,
        );
        assert_eq!(
            describe_order(&r, 0.0001),
            "order: SHORT stop @ 0.46311  SL 0.46338  TP 0.46221"
        );
    }

    #[test]
    fn levels_are_rounded_to_the_price_grid_no_float_noise() {
        // The raw resolved SL on the NZD/CHF re-entry was 0.46366999999999997;
        // at pip_size 0.0001 (5 shown decimals) it must read 0.46367.
        let r = resolved(
            Direction::Short,
            ResolvedEntry::Stop {
                trigger_price: 0.46322,
            },
            0.46366999999999997,
            0.46171,
        );
        assert_eq!(
            describe_order(&r, 0.0001),
            "order: SHORT stop @ 0.46322  SL 0.46367  TP 0.46171"
        );
    }

    #[test]
    fn index_pip_size_shows_one_decimal() {
        // A whole-point index grid (pip_size 1.0) shows one fractional digit.
        let r = resolved(
            Direction::Long,
            ResolvedEntry::Stop {
                trigger_price: 6850.0,
            },
            6800.0,
            6950.0,
        );
        assert_eq!(
            describe_order(&r, 1.0),
            "order: LONG stop @ 6850.0  SL 6800.0  TP 6950.0"
        );
    }

    #[test]
    fn long_market_and_limit_entries_label_their_type() {
        let market = resolved(
            Direction::Long,
            ResolvedEntry::Market {
                reference_price: 1.105,
            },
            1.1,
            1.115,
        );
        assert_eq!(
            describe_order(&market, 0.0001),
            "order: LONG market @ 1.10500  SL 1.10000  TP 1.11500"
        );

        let limit = resolved(
            Direction::Long,
            ResolvedEntry::Limit {
                trigger_price: 1.104,
            },
            1.1,
            1.115,
        );
        assert_eq!(
            describe_order(&limit, 0.0001),
            "order: LONG limit @ 1.10400  SL 1.10000  TP 1.11500"
        );
    }
}
