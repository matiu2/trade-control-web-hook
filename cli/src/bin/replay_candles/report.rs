//! Format the replay outcome: each fire, and — for enters — the simulated fill.
//!
//! The shell-from-fire reconstruction mirrors the worker's `dispatch_fired`
//! (an H&S Pine fire folds its latched signal onto the shell; everything else
//! gets the plain candle shell), so `simulate_fill` resolves entry/SL/TP against
//! the same levels the live dispatch would have.

use chrono::{DateTime, Utc};
use trade_control_core::intent::{Action, Direction, Resolved, ResolvedEntry, Shell};
use trade_control_engine::{SimOutcome, TradePlan, simulate_fill};

use super::brisbane::bne;
use super::replay::{Fire, Replay};

/// How a position resolved, for annotation purposes. The first three are
/// *taken* (filled) outcomes; the last two are *not-taken* — an order that
/// the price path never filled, or an entry the worker declined to place.
/// `--annotate` always draws the taken outcomes; `--annotate-unfilled` adds
/// the not-taken ones (drawn at the fire bar, muted, since there is no fill).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillKind {
    /// Filled and still open at the window's end.
    Open,
    /// Filled then hit stop-loss.
    StoppedOut,
    /// Filled then hit take-profit.
    TookProfit,
    /// A pending order that never triggered within the window. Not taken.
    NeverFilled,
    /// An entry the worker declined to place (entry past a gate level). Not taken.
    Declined,
}

impl FillKind {
    /// Was this position actually taken (an order filled)? `false` for the
    /// not-taken kinds (`NeverFilled` / `Declined`), which only have an
    /// *intended* bracket, anchored at the fire bar.
    pub fn is_taken(self) -> bool {
        matches!(self, Self::Open | Self::StoppedOut | Self::TookProfit)
    }
}

/// The resolved bracket plus simulated fill for one *filled* enter fire —
/// everything `--annotate` needs to draw the position. Computed by
/// [`resolve_fire`] so the report line and the annotator agree on the
/// same levels (the same resolution the report's order line uses).
#[derive(Debug, Clone)]
pub struct FireResult {
    pub direction: Direction,
    /// Open-time of the bar the entry actually filled on (UTC).
    pub fill_at: DateTime<Utc>,
    /// Right-edge time anchor for the drawn box: the exit bar for a closed
    /// trade, or the last replayed bar for one still open at window end.
    pub until: DateTime<Utc>,
    /// The placed entry level the fill happened at.
    pub entry_price: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub kind: FillKind,
}

/// Resolve the bracket + simulate the fill for one fire, returning a
/// [`FireResult`] **only** when it's an enter that actually filled
/// (`Open` / `StoppedOut` / `TookProfit`). Non-enters, unresolved
/// brackets, declined entries, and never-filled pending orders yield
/// `None` — they have no *taken* position to annotate.
///
/// The shell reconstruction mirrors the worker's dispatch (an H&S Pine
/// fire folds its latched signal onto the shell), so the levels match
/// what the live worker would have placed — and the report's `order:`
/// line, which resolves the same way.
pub fn resolve_fire(plan: &TradePlan, fire: &Fire) -> Option<FireResult> {
    resolve_fire_any(plan, fire).filter(|r| r.kind.is_taken())
}

/// Like [`resolve_fire`], but also returns the *not-taken* enters —
/// pending orders that never filled (`NeverFilled`) and entries the worker
/// declined (`Declined`). These have no fill bar, so the box is anchored at
/// the **fire bar** (where the order would have been placed) and runs to the
/// window end; `entry_price` is the intended placed level, not a fill.
///
/// Still `None` for non-enters and for enters whose bracket can't resolve
/// (an `Unresolved` outcome — nothing meaningful to draw).
pub fn resolve_fire_any(plan: &TradePlan, fire: &Fire) -> Option<FireResult> {
    let intent = &fire.fired.intent;
    if intent.action != Action::Enter {
        return None;
    }
    // A superseded enter never went on (its resting order was cancelled by a
    // later entry before it could fill), so there's no position to annotate —
    // not even a not-taken intended bracket: the cancel-and-replace means the
    // *replacing* entry is the one that carries the bracket forward.
    if fire.superseded {
        return None;
    }
    // A suppressed enter (paused by a news blackout) never placed an order —
    // no position to annotate, same as a superseded one.
    if fire.suppressed_by.is_some() {
        return None;
    }
    let candle = &fire.fired.candle;
    let shell = match &fire.fired.signal {
        Some(sig) => Shell::from_candle_and_signal(candle, sig),
        None => Shell::from_candle(candle),
    };
    let resolved = Resolved::from_intent(intent, &shell, plan.pip_size).ok()?;
    // For an open / not-taken trade the box runs to the last replayed bar;
    // closed trades override this with their exit bar below.
    let window_end = fire.forward.last().map(|c| c.time)?;
    // The fire bar is the not-taken anchor: an order that never filled (or was
    // declined) was still "placed" at this bar, so draw the intended bracket
    // there.
    let fire_at = candle.time;
    let (fill_at, until, entry_price, kind) =
        match simulate_fill(intent, &shell, plan.pip_size, &fire.forward) {
            SimOutcome::FilledOpen {
                fill_at,
                entry_price,
            } => (fill_at, window_end, entry_price, FillKind::Open),
            SimOutcome::StoppedOut {
                fill_at,
                entry_price,
                exit_at,
                ..
            } => (fill_at, exit_at, entry_price, FillKind::StoppedOut),
            SimOutcome::TookProfit {
                fill_at,
                entry_price,
                exit_at,
                ..
            } => (fill_at, exit_at, entry_price, FillKind::TookProfit),
            // Not taken: anchor the intended bracket at the fire bar, running
            // to the window end. The placed level (`resolved`) is the entry.
            SimOutcome::NeverFilled => (
                fire_at,
                window_end,
                resolved.entry.reference_price(),
                FillKind::NeverFilled,
            ),
            SimOutcome::Declined { .. } => (
                fire_at,
                window_end,
                resolved.entry.reference_price(),
                FillKind::Declined,
            ),
            // Nothing meaningful to draw.
            SimOutcome::Unresolved(_) => return None,
        };
    Some(FireResult {
        direction: resolved.direction,
        fill_at,
        until,
        entry_price,
        stop_loss: resolved.stop_loss,
        take_profit: resolved.take_profit,
        kind,
    })
}

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
    if intent.action == Action::Prep {
        // A prep carries no bracket of its own — preview the plan's *enter*
        // bracket resolved against this prep bar, so the journal shows the
        // SL/TP the eventual order would carry even when it never fills.
        line.push_str(&prep_would_enter_line(plan, fire));
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

    // A suppressed enter landed inside an active news-blackout pause — the gate
    // rejected it (the live worker 423s), so no order went on. Show the 0R skip
    // legibly and don't tally it.
    if let Some(blackouts) = &fire.suppressed_by {
        line.push_str(&format!(
            "    fill: SUPPRESSED — trade paused by news blackout [{}] → NO FILL / 0R\n",
            blackouts.join(", ")
        ));
        return line;
    }

    // A superseded enter had its resting order cancelled by a later entry
    // (cancel-and-replace) before it could fill, so its standalone simulated
    // fill is fiction — report the cancellation instead and don't tally it.
    if fire.superseded {
        line.push_str("    fill: SUPERSEDED — resting order cancelled by a later entry (cancel-and-replace)\n");
        return line;
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

/// The `would-enter:` preview line for a prep fire: resolve the plan's
/// *enter* bracket against the prep bar's shell and show its direction /
/// entry / SL / TP. A prep gates the entry but carries no bracket itself,
/// so this previews what order the setup is building toward — useful for
/// journaling even when the enter never fires or fills.
///
/// Falls back to a short note when the plan has no enter rule or the
/// enter can't resolve at this bar (e.g. a PinePattern enter that needs a
/// latched signal the prep bar doesn't yet have).
fn prep_would_enter_line(plan: &TradePlan, fire: &Fire) -> String {
    let Some(enter) = plan_enter_intent(plan) else {
        return "    (prep — plan has no enter rule to preview)\n".to_string();
    };
    let candle = &fire.fired.candle;
    let shell = match &fire.fired.signal {
        Some(sig) => Shell::from_candle_and_signal(candle, sig),
        None => Shell::from_candle(candle),
    };
    match Resolved::from_intent(enter, &shell, plan.pip_size) {
        Ok(resolved) => format!(
            "    would-enter: {}\n",
            describe_order(&resolved, plan.pip_size)
                .strip_prefix("order: ")
                .unwrap_or("?")
        ),
        Err(_) => "    (prep — enter bracket not resolvable at this bar yet)\n".to_string(),
    }
}

/// The first enter intent in the plan, if any. The bundle has exactly one
/// enter rule (`05-enter`); the rest are vetos/preps.
fn plan_enter_intent(plan: &TradePlan) -> Option<&trade_control_core::intent::Intent> {
    plan.rules
        .iter()
        .map(|r| &r.intent)
        .find(|i| i.action == Action::Enter)
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
        } => {
            // When the stop landed at the entry price, break-even management
            // (BUG-replay-no-breakeven-stop-at-50pct) moved it there after a
            // candle closed past 50%-to-TP — a 0R scratch, not a −1R loss.
            let label = if (exit_price - entry_price).abs() < 1e-9 {
                "BREAK-EVEN (SL→BE) — in @"
            } else {
                "STOPPED OUT — in @"
            };
            format!(
                "fill: {label} {entry_price} ({}) → SL {exit_price} ({})",
                bne(*fill_at),
                bne(*exit_at)
            )
        }
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
            recover_entry: None,
            breakeven: None,
        }
    }

    #[test]
    fn taken_kinds_are_filled_not_taken_kinds_are_not() {
        assert!(FillKind::Open.is_taken());
        assert!(FillKind::StoppedOut.is_taken());
        assert!(FillKind::TookProfit.is_taken());
        assert!(!FillKind::NeverFilled.is_taken());
        assert!(!FillKind::Declined.is_taken());
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
