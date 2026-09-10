//! The pure decision behind the break-even watcher — everything between "here
//! are the candles the broker returned" and "amend the stop to X".
//!
//! Split out of [`crate::breakeven_watch`] (which is now a thin broker wrapper)
//! so the whole decision — window bounding, target selection, and the noise
//! floor — is unit-testable at the level the live cron actually calls, not just
//! at the pure helper below it.
//!
//! # Why this module exists at all
//!
//! `BUG-breakeven-arms-off-pre-fill-history.md`. The live watcher fetched a
//! 500-bar lookback (≈83 days on H4), filtered only on "has this bar closed",
//! and armed break-even off **any** of them — so a NZD_CAD short that filled on
//! 2026-08-10 was armed by a bar that closed on 2026-07-30, eleven days before
//! it existed, and scratched 6 minutes after filling. It then amended the stop
//! to the *order trigger* (0.82046) while the position had filled at 0.82043 —
//! for a short, three ticks on the losing side of the fill, i.e. a guaranteed
//! loss dressed as a scratch.
//!
//! The offline replay (`fill_sim::simulate_fill`) never had either defect: it
//! walks only `fill.rest` — the bars from the fill bar onward — and arms and
//! targets off `fill.entry_price`, the actual fill. This module brings live to
//! match replay (the divergence *was* the bug), so both sides now answer the
//! same two questions the same way:
//!
//! 1. **Which bars may arm?** Only bars that CLOSED at or after the fill —
//!    which includes the bar the fill landed inside, exactly as `fill.rest`
//!    does. See [`armable_candles`] for why the bound is the close, not the
//!    open.
//! 2. **Where does the stop go?** The price the position actually filled at.

use chrono::{DateTime, Duration, Utc};
use trade_control_core::broker::{Candle, Granularity, OpenPosition};
use trade_control_core::intent::Breakeven;
use trade_control_core::signals::{atr_length_for, wilder_atr};
use trade_control_core::state::BreakevenSnapshot;

/// How close to current price a break-even stop may land before the amend is
/// refused as noise, expressed as a fraction of the trade's own ATR.
///
/// # Why a floor, and why this number
///
/// The repo already holds the principle that a stop dominated by transaction
/// cost is not a stop: `intent::sl_spread_floor` rejects an *entry* whose SL
/// sits within `10 ×` the live spread, on the grounds that "the spread alone
/// can stop the trade out before any real adverse move". A break-even amend is
/// the same act — it replaces a stop — and had no such check, which is how the
/// incident's stop landed **1.6% of ATR** from market and filled inside the
/// same broker batch that created it.
///
/// This watcher sees only *mid* candles, so it cannot read a bid-ask spread to
/// reuse that constant directly. ATR is the volatility unit it *can* compute
/// from the candles it already fetched, so the floor is expressed in ATR.
/// `0.1 × ATR` is the value the bug report proposed and is deliberately
/// permissive: a legitimately-armed break-even sits ~50% of the way to TP from
/// current price, which is many multiples of ATR — orders of magnitude clear of
/// this line. It is a tripwire for absurdity, not a tuning knob, and it exists
/// to make the *next* mis-derived target loud instead of silent.
///
/// **Fail-open, deliberately.** When the ATR cannot be computed (a window
/// shorter than [`atr_length_for`]) the floor is not applied — the same
/// discipline as `sl_spread_floor_violation`, which treats a degenerate spread
/// as unjudgeable rather than fabricating a rejection. The floor is
/// defence-in-depth behind the window bound and the fill-priced target; it must
/// never become the thing that silently suppresses a correct break-even.
pub const BREAKEVEN_MIN_ATR_FRACTION: f64 = 0.1;

/// Everything the break-even decision needs that isn't the candle window.
///
/// A named struct rather than a positional call: the snapshot's `entry_price`
/// and `take_profit`, the position's `entry_price`, and `current_stop` are four
/// same-typed `f64` prices around one trade, and a transposition between any
/// two of them is a live-money bug that compiles clean. The trigger-vs-fill
/// confusion in Defect 2 is precisely that kind of swap.
pub struct BreakevenInputs<'a> {
    pub snapshot: &'a BreakevenSnapshot,
    pub position: &'a OpenPosition,
    /// The stop currently attached at the broker.
    pub current_stop: f64,
}

/// What the watcher should do with one open position this tick.
#[derive(Debug, Clone, PartialEq)]
pub enum BreakevenDecision {
    /// Nothing to do — not armed yet, or already at break-even.
    Hold,
    /// Amend the broker stop to `new_stop`. `armed_by` is the close that armed
    /// it and `at` that bar's time, both for the log line.
    Amend {
        new_stop: f64,
        armed_by: f64,
        at: DateTime<Utc>,
    },
    /// The decision cannot be made safely and the position is left alone. Every
    /// variant here is a **loud** skip: silence is what let the incident run.
    Blocked(BreakevenBlock),
}

/// Why a break-even decision was refused. Each is logged at `error`/`warn` —
/// none is a routine no-op (that is [`BreakevenDecision::Hold`]).
#[derive(Debug, Clone, PartialEq)]
pub enum BreakevenBlock {
    /// The broker did not report when the position filled, so there is no
    /// honest lower bound on the candle window.
    ///
    /// **Fails closed on purpose.** Falling back to "no bound" is precisely the
    /// defect: it re-admits months of pre-fill history. Falling back to the
    /// order's `placed_at` is *also* wrong — a resting stop/limit order can sit
    /// for hours before filling (46 minutes on the incident), so `placed_at`
    /// still admits bars the position never lived through. Break-even is an
    /// optimisation on top of a stop that is already protecting the trade;
    /// declining to arm it costs at most the difference between a scratch and a
    /// full stop-out, while arming it wrongly costs a winner. Skip and shout.
    NoFillTime,
    /// The amend would land within [`BREAKEVEN_MIN_ATR_FRACTION`] × ATR of the
    /// most recent close — a stop inside noise, which is not a scratch.
    /// Carries the numbers so the log line can be read without a debugger.
    InsideNoise {
        new_stop: f64,
        reference_price: f64,
        distance: f64,
        floor: f64,
    },
}

/// The price a break-even stop should target, and where it came from.
///
/// The break-even *target* is by definition "the price this position was
/// opened at" — so it must be the **fill**, which is what the broker reports on
/// the open position, not the resolved trigger snapshotted at placement. A stop
/// order's trigger and its fill are different numbers whenever price gaps or
/// runs through the level, and on a short a fill *better* than trigger puts the
/// trigger-priced stop above the fill: a guaranteed loss.
fn target_entry(inputs: &BreakevenInputs<'_>) -> f64 {
    inputs
        .position
        .entry_price
        .unwrap_or(inputs.snapshot.entry_price)
}

/// The closed candles this position may arm break-even off: bars that had
/// **closed** as of `now`, and whose close falls at or after the fill.
///
/// Both bounds matter and they ask different questions. `c.time + bar <= now`
/// asks "has this bar finished?"; `c.time + bar > fill_at` asks "did this bar
/// close while we were in the trade?". The live watcher only ever asked the
/// first, which is why ~83 days of pre-fill history could arm a six-minute-old
/// position (`BUG-breakeven-arms-off-pre-fill-history.md`, Defect 1).
///
/// # Why the lower bound is the bar's CLOSE, not its open
///
/// Break-even arms on a **close**, so the question is whether that close is
/// evidence the position ran anywhere — and a close that happens after the fill
/// is exactly that, even on the bar the fill landed inside. Bounding on the
/// bar's *open* instead would discard the fill bar's own close, which is real
/// post-fill information.
///
/// This is also what keeps live equal to replay. `fill_sim::find_fill` returns
/// `rest: &candles[i + 1..]` where `i + 1` is the **fill bar itself** — its
/// comment is explicit that the post-fill window "**includes** the fill bar" —
/// and `simulate_fill` arms break-even off every bar in `rest`. Bounding here on
/// the bar open would have made live strictly stricter than replay on exactly
/// one bar per trade, which is the sort of quiet one-bar divergence that made
/// this bug invisible to the fixture corpus in the first place.
///
/// A bar that closed at or before the fill is excluded either way: nothing it
/// shows happened while the position existed.
fn armable_candles(
    candles: Vec<Candle>,
    granularity: Granularity,
    fill_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Vec<Candle> {
    let bar = Duration::seconds(granularity.seconds());
    candles
        .into_iter()
        .filter(|c| c.time + bar <= now && c.time + bar > fill_at)
        .collect()
}

/// Decide what to do with one open position, given the candles the broker
/// returned for it. Pure: no broker, no clock, no store.
pub fn decide(
    inputs: &BreakevenInputs<'_>,
    candles: Vec<Candle>,
    now: DateTime<Utc>,
) -> BreakevenDecision {
    let Some(fill_at) = inputs.position.opened_at else {
        return BreakevenDecision::Blocked(BreakevenBlock::NoFillTime);
    };
    let direction = inputs.position.direction;
    let armable = armable_candles(candles, inputs.snapshot.granularity, fill_at, now);
    // The close that ran furthest toward TP since the fill. Break-even is
    // latched, so an arm on a bar that has since retraced must not be missed.
    let Some(best) = armable.iter().copied().reduce(|a, b| {
        if Breakeven::more_progressed(direction, a.c, b.c) == b.c {
            b
        } else {
            a
        }
    }) else {
        // No closed post-fill bar yet — the normal state for a fresh fill.
        return BreakevenDecision::Hold;
    };
    let entry = target_entry(inputs);
    let Some(new_stop) = inputs.snapshot.rule.decide_move(
        direction,
        entry,
        inputs.snapshot.take_profit,
        inputs.current_stop,
        best.c,
    ) else {
        return BreakevenDecision::Hold;
    };
    // Defence-in-depth: a break-even that lands inside noise is not a scratch.
    // Measured against the LAST armable close (the most recent price we have),
    // not the arming close, which may be far from current price.
    if let Some(block) = noise_violation(&armable, new_stop, inputs.snapshot.granularity) {
        return BreakevenDecision::Blocked(block);
    }
    BreakevenDecision::Amend {
        new_stop,
        armed_by: best.c,
        at: best.time,
    }
}

/// `Some(block)` when `new_stop` sits within [`BREAKEVEN_MIN_ATR_FRACTION`] ×
/// ATR of the latest close. `None` when it clears the floor **or** when the ATR
/// is unjudgeable (fail-open — see [`BREAKEVEN_MIN_ATR_FRACTION`]).
fn noise_violation(
    armable: &[Candle],
    new_stop: f64,
    granularity: Granularity,
) -> Option<BreakevenBlock> {
    let latest = armable.last()?;
    let atr = wilder_atr(armable, atr_length_for(granularity))?;
    if !atr.is_finite() || atr <= 0.0 {
        return None;
    }
    let floor = BREAKEVEN_MIN_ATR_FRACTION * atr;
    let distance = (new_stop - latest.c).abs();
    if distance < floor {
        Some(BreakevenBlock::InsideNoise {
            new_stop,
            reference_price: latest.c,
            distance,
            floor,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_core::intent::Direction;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    /// One H4 bar. `c` is the close — the only field the arm reads — but the
    /// OHLC is kept coherent so the ATR floor sees a realistic range.
    fn bar(time: &str, close: f64) -> Candle {
        Candle {
            time: ts(time),
            o: close,
            h: close + 0.0010,
            l: close - 0.0010,
            c: close,
        }
    }

    /// `n` consecutive H4 bars starting at `start`, all closing at `close`.
    /// Long enough to warm the H4 ATR (36 bars) so the noise floor is
    /// judgeable — generated arithmetically so the fixture can't run off the
    /// end of a month.
    fn warm_bars(start: &str, n: i64, close: f64) -> Vec<Candle> {
        let t0 = ts(start);
        (0..n)
            .map(|i| Candle {
                time: t0 + Duration::hours(4 * i),
                o: close,
                h: close + 0.0010,
                l: close - 0.0010,
                c: close,
            })
            .collect()
    }

    /// The NZD_CAD H4 short from `BUG-breakeven-arms-off-pre-fill-history.md`:
    /// stop-order trigger 0.82046, actual fill 0.82043, TP 0.81531, designed SL
    /// 0.82527. The 50%-to-TP arming level is ≈ 0.81787.
    fn incident_snapshot() -> BreakevenSnapshot {
        BreakevenSnapshot {
            rule: Breakeven::at_half(),
            // The TRIGGER, exactly as placement snapshots it today.
            entry_price: 0.82046,
            take_profit: 0.81531,
            granularity: Granularity::H4,
        }
    }

    fn incident_position(
        entry_price: Option<f64>,
        opened_at: Option<DateTime<Utc>>,
    ) -> OpenPosition {
        OpenPosition {
            instrument: "NZD_CAD".into(),
            direction: Direction::Short,
            stop_loss: Some(0.82527),
            take_profit: Some(0.81531),
            position_id: "2321".into(),
            order_id: "2321".into(),
            stake: 876_934.0,
            entry_price,
            opened_at,
        }
    }

    /// 40 H4 bars ending just before the fill, every one of them closing far
    /// below the 0.81787 arming level — the July price action that armed
    /// break-even on the real trade. 40 bars clears the H4 ATR warmup (36) so
    /// the noise floor is judgeable and cannot be what suppresses an arm.
    fn pre_fill_history() -> Vec<Candle> {
        (0..40)
            .map(|i| {
                let day = 1 + i / 6;
                let hour = (i % 6) * 4;
                bar(
                    &format!("2026-07-{day:02}T{hour:02}:00:00Z"),
                    // Well below the arming level — every one of these arms a
                    // short if it is allowed to count.
                    0.80500 + f64::from(i) * 0.00001,
                )
            })
            .collect()
    }

    // ---------------------------------------------------------------- Defect 1

    /// **The incident.** A short fills at 23:46 on 2026-08-10 having moved
    /// nowhere; the only bars that ran past the arming level closed in July,
    /// eleven days earlier. Break-even must NOT arm.
    ///
    /// This is the whole bug in one assertion: with the window bounded at the
    /// fill, none of those bars is armable, so there is no `best_close` and the
    /// decision is `Hold`.
    #[test]
    fn pre_fill_bars_never_arm_breakeven() {
        let snap = incident_snapshot();
        let pos = incident_position(Some(0.82043), Some(ts("2026-08-10T23:46:00Z")));
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 0.82527,
        };
        // Every armable-looking bar predates the fill.
        let candles = pre_fill_history();
        let decision = decide(&inputs, candles, ts("2026-08-10T23:52:00Z"));
        assert_eq!(
            decision,
            BreakevenDecision::Hold,
            "July bars must not arm a position that filled on 10 August",
        );
    }

    /// The mirror: the SAME bars, but now the position filled *before* them, so
    /// they are genuinely part of the trade's life and must arm.
    ///
    /// Without this pair the previous test proves nothing — a `decide` that
    /// never armed anything would also pass it.
    #[test]
    fn post_fill_bars_do_arm_breakeven() {
        let snap = incident_snapshot();
        let pos = incident_position(Some(0.82043), Some(ts("2026-06-30T00:00:00Z")));
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 0.82527,
        };
        let decision = decide(&inputs, pre_fill_history(), ts("2026-08-10T23:52:00Z"));
        assert!(
            matches!(decision, BreakevenDecision::Amend { .. }),
            "the same bars, now post-fill, must arm: {decision:?}",
        );
    }

    /// The bar the fill landed *inside* DOES arm — its close happened after the
    /// fill, so it is genuine post-fill evidence.
    ///
    /// This pins **replay parity**, not just a preference. `fill_sim::find_fill`
    /// returns `rest: &candles[i + 1..]` where `i + 1` is the fill bar, and its
    /// comment says so explicitly ("the post-fill search **includes** the fill
    /// bar itself"); `simulate_fill` arms break-even off every bar in `rest`.
    /// Excluding it here would make live one bar stricter than replay on every
    /// trade — exactly the kind of quiet divergence the fixture corpus cannot
    /// see.
    #[test]
    fn the_bar_the_fill_landed_inside_arms_matching_replay() {
        let snap = incident_snapshot();
        // Fill at 23:46 — inside the 20:00 H4 bar, which closes at 00:00.
        let pos = incident_position(Some(0.82043), Some(ts("2026-08-10T23:46:00Z")));
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 0.82527,
        };
        let candles = vec![bar("2026-08-10T20:00:00Z", 0.80500)];
        assert!(
            matches!(
                decide(&inputs, candles, ts("2026-08-11T04:00:00Z")),
                BreakevenDecision::Amend { .. }
            ),
            "the fill bar's own close is post-fill evidence, as it is in replay",
        );
    }

    /// The bar BEFORE the fill bar does not arm — its close predates the fill
    /// entirely, so nothing it shows happened while the position existed. This
    /// is the boundary the previous test's twin must not swallow.
    #[test]
    fn the_bar_that_closed_before_the_fill_does_not_arm() {
        let snap = incident_snapshot();
        let pos = incident_position(Some(0.82043), Some(ts("2026-08-10T23:46:00Z")));
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 0.82527,
        };
        // The 16:00 H4 bar closed at 20:00 — before the 23:46 fill.
        let candles = vec![bar("2026-08-10T16:00:00Z", 0.80500)];
        assert_eq!(
            decide(&inputs, candles, ts("2026-08-11T04:00:00Z")),
            BreakevenDecision::Hold,
            "a bar that closed before the fill is not post-fill evidence",
        );
    }

    /// An unclosed bar still doesn't arm — the pre-existing "has this bar
    /// closed" bound survives the new fill bound.
    #[test]
    fn an_unclosed_post_fill_bar_does_not_arm() {
        let snap = incident_snapshot();
        let pos = incident_position(Some(0.82043), Some(ts("2026-08-10T23:46:00Z")));
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 0.82527,
        };
        // The 00:00 H4 bar opens after the fill but closes at 04:00; `now` is
        // 02:00, so it is still forming.
        let candles = vec![bar("2026-08-11T00:00:00Z", 0.80500)];
        assert_eq!(
            decide(&inputs, candles, ts("2026-08-11T02:00:00Z")),
            BreakevenDecision::Hold,
            "an in-progress bar must not arm break-even",
        );
    }

    /// No fill time from the broker ⇒ refuse, loudly. Deliberately NOT a
    /// fallback to the unbounded window (the defect) nor to `placed_at` (which
    /// still admits pre-fill bars — the order rested 46 minutes on the
    /// incident).
    #[test]
    fn a_position_with_no_fill_time_is_blocked_not_armed() {
        let snap = incident_snapshot();
        let pos = incident_position(Some(0.82043), None);
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 0.82527,
        };
        assert_eq!(
            decide(&inputs, pre_fill_history(), ts("2026-08-10T23:52:00Z")),
            BreakevenDecision::Blocked(BreakevenBlock::NoFillTime),
            "unknown fill time must block, never fall through to an unbounded window",
        );
    }

    // ---------------------------------------------------------------- Defect 2

    /// **The second half of the incident.** The break-even stop must be the
    /// price the position FILLED at (0.82043), not the order trigger the
    /// snapshot froze at placement (0.82046).
    ///
    /// For this short the two differ by 3 ticks in the direction that matters:
    /// a stop at the trigger sits *above* the fill, i.e. a guaranteed loss.
    #[test]
    fn breakeven_targets_the_broker_fill_not_the_placement_trigger() {
        let snap = incident_snapshot();
        let pos = incident_position(Some(0.82043), Some(ts("2026-08-10T23:46:00Z")));
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 0.82527,
        };
        // A genuine post-fill run past the 0.81787 arming level.
        let candles = vec![
            bar("2026-08-11T00:00:00Z", 0.81900),
            bar("2026-08-11T04:00:00Z", 0.81700),
        ];
        match decide(&inputs, candles, ts("2026-08-11T08:00:00Z")) {
            BreakevenDecision::Amend { new_stop, .. } => {
                assert!(
                    (new_stop - 0.82043).abs() < 1e-9,
                    "expected the fill 0.82043, got {new_stop} (0.82046 is the trigger — the bug)",
                );
                assert!(
                    new_stop <= 0.82043,
                    "a short's break-even stop must never sit above its fill",
                );
            }
            other => panic!("expected an amend, got {other:?}"),
        }
    }

    /// The mirror for a long: a fill *worse* than trigger must not leave the
    /// stop below the fill. Same defect, opposite sign — the incident's shape
    /// only exposes one side.
    #[test]
    fn a_long_filled_worse_than_trigger_targets_its_own_fill() {
        let snap = BreakevenSnapshot {
            rule: Breakeven::at_half(),
            entry_price: 1.1000, // trigger
            take_profit: 1.1200,
            granularity: Granularity::H4,
        };
        let pos = OpenPosition {
            instrument: "EUR_USD".into(),
            direction: Direction::Long,
            stop_loss: Some(1.0900),
            take_profit: Some(1.1200),
            position_id: "p1".into(),
            order_id: "p1".into(),
            stake: 1000.0,
            entry_price: Some(1.1005), // slipped 5 ticks against us
            opened_at: Some(ts("2026-08-10T20:00:00Z")),
        };
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 1.0900,
        };
        let candles = vec![bar("2026-08-11T00:00:00Z", 1.1150)];
        match decide(&inputs, candles, ts("2026-08-11T08:00:00Z")) {
            BreakevenDecision::Amend { new_stop, .. } => {
                assert!(
                    (new_stop - 1.1005).abs() < 1e-9,
                    "expected the fill 1.1005, got {new_stop} (1.1000 is the trigger — the bug)",
                );
                assert!(
                    new_stop >= 1.1005,
                    "a long's break-even stop must never sit below its fill",
                );
            }
            other => panic!("expected an amend, got {other:?}"),
        }
    }

    /// When the broker reports no fill price, the placement snapshot is the
    /// only number available and is used — the fill *time* is what fails
    /// closed, not the fill price. Losing break-even entirely because a broker
    /// omitted a price would be a worse trade-off than a possibly-stale target
    /// on a stop that is already correct in the overwhelming majority of fills.
    #[test]
    fn a_missing_broker_fill_price_falls_back_to_the_snapshot() {
        let snap = incident_snapshot();
        let pos = incident_position(None, Some(ts("2026-08-10T23:46:00Z")));
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 0.82527,
        };
        let candles = vec![bar("2026-08-11T00:00:00Z", 0.81700)];
        match decide(&inputs, candles, ts("2026-08-11T08:00:00Z")) {
            BreakevenDecision::Amend { new_stop, .. } => assert!(
                (new_stop - 0.82046).abs() < 1e-9,
                "expected the snapshot fallback 0.82046, got {new_stop}",
            ),
            other => panic!("expected an amend, got {other:?}"),
        }
    }

    // ------------------------------------------------------------- Noise floor

    /// A break-even target that lands on top of current price is refused. This
    /// is the shape the incident's amend had: a stop 3 ticks from market, which
    /// filled inside the same broker batch that created it.
    #[test]
    fn a_breakeven_landing_on_top_of_market_is_blocked() {
        // Short whose fill is (absurdly) right where price now sits, but which
        // legitimately armed on an earlier bar. Only the floor can catch this.
        let snap = BreakevenSnapshot {
            rule: Breakeven::at_half(),
            entry_price: 0.81700,
            take_profit: 0.81000,
            granularity: Granularity::H4,
        };
        let mut pos = incident_position(Some(0.81700), Some(ts("2026-07-01T00:00:00Z")));
        pos.stop_loss = Some(0.82500);
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 0.82500,
        };
        // 40 warm bars so the ATR is judgeable, then an arming bar, then price
        // returns to 0.81700 — right on top of the break-even target.
        let mut candles = warm_bars("2026-07-01T00:00:00Z", 40, 0.81800);
        candles.push(bar("2026-08-11T00:00:00Z", 0.81300)); // arms (< 0.81350)
        candles.push(bar("2026-08-11T04:00:00Z", 0.81700)); // back on the target
        match decide(&inputs, candles, ts("2026-08-11T08:00:00Z")) {
            BreakevenDecision::Blocked(BreakevenBlock::InsideNoise {
                distance, floor, ..
            }) => assert!(distance < floor, "{distance} should be under {floor}"),
            other => panic!("expected an InsideNoise block, got {other:?}"),
        }
    }

    /// The floor must not suppress a NORMAL break-even. A legitimately-armed BE
    /// sits ~50% of the way to TP from current price — many ATRs clear.
    #[test]
    fn a_normal_breakeven_clears_the_noise_floor() {
        let snap = incident_snapshot();
        let pos = incident_position(Some(0.82043), Some(ts("2026-07-01T00:00:00Z")));
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 0.82527,
        };
        let mut candles = warm_bars("2026-07-01T00:00:00Z", 40, 0.82000);
        candles.push(bar("2026-08-11T00:00:00Z", 0.81700)); // armed, far from BE
        match decide(&inputs, candles, ts("2026-08-11T08:00:00Z")) {
            BreakevenDecision::Amend { new_stop, .. } => {
                assert!((new_stop - 0.82043).abs() < 1e-9)
            }
            other => panic!("a normal break-even must not be blocked: {other:?}"),
        }
    }

    /// Fail-open: with too few bars to compute the ATR the floor is not
    /// applied, matching `sl_spread_floor_violation`'s "a degenerate spread is
    /// unjudgeable" discipline. The floor is defence-in-depth; it must never be
    /// the thing that silently withholds a correct break-even.
    #[test]
    fn an_unjudgeable_atr_does_not_block_the_amend() {
        let snap = incident_snapshot();
        let pos = incident_position(Some(0.82043), Some(ts("2026-08-10T20:00:00Z")));
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 0.82527,
        };
        // Two bars only — far short of H4's 36-bar ATR warmup. The second sits
        // right on the break-even target, which WOULD violate a judgeable floor.
        let candles = vec![
            bar("2026-08-11T00:00:00Z", 0.81700),
            bar("2026-08-11T04:00:00Z", 0.82043),
        ];
        assert!(
            matches!(
                decide(&inputs, candles, ts("2026-08-11T08:00:00Z")),
                BreakevenDecision::Amend { .. }
            ),
            "an unjudgeable ATR must fail open",
        );
    }

    // ------------------------------------------------------- Latching / basics

    /// Break-even is latched: an arm on a bar that has since retraced still
    /// counts, so `decide` folds for the furthest-toward-TP close rather than
    /// reading the latest.
    #[test]
    fn a_retraced_arm_still_counts() {
        let snap = incident_snapshot();
        let pos = incident_position(Some(0.82043), Some(ts("2026-08-10T20:00:00Z")));
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 0.82527,
        };
        let candles = vec![
            bar("2026-08-11T00:00:00Z", 0.81700), // armed here
            bar("2026-08-11T04:00:00Z", 0.81950), // retraced, no longer past
        ];
        match decide(&inputs, candles, ts("2026-08-11T08:00:00Z")) {
            BreakevenDecision::Amend { armed_by, at, .. } => {
                assert!((armed_by - 0.81700).abs() < 1e-9);
                assert_eq!(at, ts("2026-08-11T00:00:00Z"));
            }
            other => panic!("a latched arm must survive a retrace: {other:?}"),
        }
    }

    /// Idempotent: once the stop is already at break-even the decision holds,
    /// so re-running every tick makes no redundant broker call.
    #[test]
    fn an_already_breakeven_stop_holds() {
        let snap = incident_snapshot();
        let pos = incident_position(Some(0.82043), Some(ts("2026-08-10T20:00:00Z")));
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            // Already at the fill.
            current_stop: 0.82043,
        };
        let candles = vec![bar("2026-08-11T00:00:00Z", 0.81700)];
        assert_eq!(
            decide(&inputs, candles, ts("2026-08-11T08:00:00Z")),
            BreakevenDecision::Hold,
        );
    }

    /// An empty pull is a hold, not a block — the ordinary "just filled, no
    /// closed bar yet" state.
    #[test]
    fn no_candles_at_all_is_a_hold() {
        let snap = incident_snapshot();
        let pos = incident_position(Some(0.82043), Some(ts("2026-08-10T23:46:00Z")));
        let inputs = BreakevenInputs {
            snapshot: &snap,
            position: &pos,
            current_stop: 0.82527,
        };
        assert_eq!(
            decide(&inputs, Vec::new(), ts("2026-08-11T00:00:00Z")),
            BreakevenDecision::Hold,
        );
    }
}
