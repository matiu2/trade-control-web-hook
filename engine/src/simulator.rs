//! A pure candle-driven fill simulator for tick-bundle replay.
//!
//! The pure-evaluator replay (`trade-control replay`) diffs what the engine
//! *decided* (`PlanEval`). This module answers the next question: given a fired
//! **enter** intent and the candles that followed, *what would the broker have
//! done* — did the order fill, and did the position exit at its stop or target?
//!
//! It is the replay half of the roadmap's [broker simulator](../../roadmap/src/
//! broker-simulator.md), kept deliberately dumb (v1): resolve the intent's
//! entry/SL/TP via the pure [`Resolved::from_intent`], then walk the recorded
//! candles — a pending stop/limit fills when a candle's range crosses its
//! trigger; a market entry fills at once; after a fill, the first candle whose
//! range touches the stop or the target closes the position. Candles come from
//! the recorded bundle (answering the doc's open question), so the simulation is
//! deterministic and needs no broker.
//!
//! What it is **not**: it does not run the worker's `run_enter` dispatch (that
//! lives in the worker cdylib and returns a `worker::Response` that panics
//! off-wasm), so it doesn't reproduce sizing, the seen-id index, or gate
//! rejections — only the price-path fill/exit. Replaying the recorded
//! `dispatch_outcomes` through the real handlers needs the `Response` decouple
//! and is a separate, later task.
//!
//! ## Known optimism vs a real broker (deliberate v1 simplifications)
//!
//! These make the sim report outcomes a *little* better than reality. They are
//! intentional — modelling them needs live quotes / KV state the offline replay
//! doesn't have — but a future debugger should know the replay is optimistic on:
//!
//! - **Gap fills priced at the resting level.** A bar that gaps *through* a stop
//!   trigger / SL fills at the gapped book extreme on a real broker, not the
//!   requested level. We record the placed level (`book_crosses` is a boolean
//!   touch). Optimistic for stop entries and stop-loss exits.
//! - **Market entry fills at mid.** `ResolvedEntry::Market` books the mid close,
//!   not the spread-crossed price (buy the ask / sell the bid). Optimistic by the
//!   half-spread.
//! - **No KV-state or live-quote gates.** The worker's `run_enter` also applies
//!   cooldown, KV vetos, prep ordering, account caps, the `allow_entry` script,
//!   the seen-id replay check, the SL≥10×spread floor, spread-blackout,
//!   market-hours blackout, and news windows. The replay applies none of these
//!   (only the at-entry-level veto below), so it can report fires/fills the
//!   worker would reject.
//!
//! What *is* modelled faithfully (don't "simplify" these away): the fire-bar skip
//! (a pending order can't fill on the bar that fired it), same-bar fill-and-stop
//! (the fill bar is in the exit search, pessimistic on SL/TP ties), per-bar
//! bid/ask book selection, and bar-expiry (`expiry_bars` bounds the fill window).

use trade_control_core::broker::BidAskCandle;
use trade_control_core::intent::{Direction, Intent, Resolved, ResolvedEntry, Shell};
use trade_control_core::sweep_gate::{SweepReason, bar_expiry_due, breach_detected};

/// What the simulator decided happened to one fired enter over the candle path.
#[derive(Debug, Clone, PartialEq)]
pub enum SimOutcome {
    /// The pending order never filled within the recorded candles.
    NeverFilled,
    /// Filled, but no SL/TP touched within the recorded candles — still open.
    FilledOpen {
        /// Open-time of the candle the entry filled on.
        fill_at: chrono::DateTime<chrono::Utc>,
        entry_price: f64,
    },
    /// Filled, then the stop-loss was hit.
    StoppedOut {
        fill_at: chrono::DateTime<chrono::Utc>,
        entry_price: f64,
        exit_at: chrono::DateTime<chrono::Utc>,
        exit_price: f64,
    },
    /// Filled, then the take-profit was hit.
    TookProfit {
        fill_at: chrono::DateTime<chrono::Utc>,
        entry_price: f64,
        exit_at: chrono::DateTime<chrono::Utc>,
        exit_price: f64,
    },
    /// The intent couldn't be resolved to concrete levels (not an enter, M/W
    /// not armed, invalid geometry, …). Carries the resolver's reason.
    Unresolved(String),
    /// The worker's at-entry level veto (Bug #12) would have rejected the
    /// entry: the resolved entry price is already past a baked
    /// `entry_level_veto` (pcl-exhausted / invalidation). No order placed.
    /// Carries the veto name (`too-low` / `too-high`). `simulate_fill`
    /// short-circuits here before any fill, mirroring `run_enter`'s gate.
    Declined { name: String },
}

/// Simulate one fired enter `intent` (with its triggering `candle` folded into a
/// `Shell`) against the forward candle path `candles` (ascending, the bundle's
/// `new_candles`/`detector_window`). `pip_size` is the plan's pip size, needed to
/// resolve pip-offset entry/SL levels.
///
/// The candles carry **real per-bar bid/ask books** ([`BidAskCandle`]), so the
/// fill reproduces the broker's actual spread (which widens at session opens and
/// around news) instead of a flat synthetic half-spread. You **buy at the ask,
/// sell at the bid**, so the book each leg touches depends on direction:
///
/// - **Short** (sell to open, buy to close): entry fills when the **bid** range
///   reaches the trigger; SL/TP close when the **ask** range reaches them.
/// - **Long** (buy to open, sell to close): mirror — entry on the **ask** range,
///   exits on the **bid** range.
///
/// The engine still *resolves* and evaluates on MID (the worker places every
/// level at mid); only the **fill test** here uses the relevant book side, which
/// is where the real spread lives. Recorded fill/exit prices are the placed
/// level (the resting order's price), not the book extreme that touched it.
///
/// When the data source only serves mid (bid == ask == mid per bar), this
/// degrades cleanly to exact-level mid fills.
pub fn simulate_fill(
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
) -> SimOutcome {
    let resolved = match Resolved::from_intent(intent, shell, pip_size) {
        Ok(r) => r,
        Err(err) => return SimOutcome::Unresolved(err.to_string()),
    };
    let dir = resolved.direction;

    // At-entry level veto (Bug #12) — the worker's `run_enter` rejects an
    // entry already past a baked pcl-exhausted / invalidation level before any
    // placement, independent of the cross-event guard. `simulate_fill` doesn't
    // run `run_enter`, so mirror that gate here: a past level → no fill.
    let entry_ref_price = resolved.entry.reference_price();
    if let Some(elv) = intent
        .entry_level_vetos
        .iter()
        .find(|elv| elv.is_past(entry_ref_price))
    {
        return SimOutcome::Declined {
            name: elv.name.clone(),
        };
    }

    // Phase 1 — find the fill (shared with `breakeven_armed_at`).
    let Some(fill) = find_fill(&resolved, intent, shell, dir, candles) else {
        return SimOutcome::NeverFilled;
    };
    let (fill_at, entry_price, rest) = (fill.fill_at, fill.entry_price, fill.rest);

    // Phase 2 — after the fill, the first candle that touches SL or TP closes
    // the position. The close is the *opposite* book side from entry (short buys
    // back on the ask, long sells on the bid). If both are touched in the same
    // candle we can't tell the intrabar order from a closed bar, so we
    // conservatively call it the stop (the worse outcome) — matches the
    // simulator doc's "exact-level, pessimistic on ambiguity" stance.
    //
    // Break-even management (BUG-replay-no-breakeven-stop-at-50pct): if the
    // enter carries `breakeven`, the active stop starts at the resolved SL and
    // moves to the entry price once a candle *closes* past the 50%-to-TP level.
    // Latched / one-way. The operator's same-bar rule: BE arms on a close, so
    // the moved stop is live from the **next** bar — on the arming bar the
    // original (or already-moved) stop still applies, mirroring the broker's
    // resting stop. We therefore test the exit against `active_stop` first, then
    // arm BE from this candle's close for subsequent bars.
    let exit_book = book_for(Leg::Exit, dir);
    let be_arms_at = resolved
        .breakeven
        .map(|be| be.arms_at(entry_price, resolved.take_profit));
    let mut active_stop = resolved.stop_loss;
    for c in rest {
        let hit_sl = book_crosses(c, exit_book, active_stop);
        let hit_tp = book_crosses(c, exit_book, resolved.take_profit);
        match (hit_sl, hit_tp) {
            (true, _) => {
                return SimOutcome::StoppedOut {
                    fill_at,
                    entry_price,
                    exit_at: c.time,
                    exit_price: active_stop,
                };
            }
            (false, true) => {
                return SimOutcome::TookProfit {
                    fill_at,
                    entry_price,
                    exit_at: c.time,
                    exit_price: resolved.take_profit,
                };
            }
            (false, false) => {}
        }
        // Arm break-even for the next bar onward when this candle CLOSES past
        // the 50% level. Latched: once moved to entry it never reverts (a long's
        // entry >= original SL, a short's entry <= it, so `active_stop` only
        // tightens).
        if let (Some(be), Some(level)) = (resolved.breakeven, be_arms_at)
            && be.close_arms(dir, level, c.c)
        {
            active_stop = be.target_stop(entry_price);
        }
    }

    SimOutcome::FilledOpen {
        fill_at,
        entry_price,
    }
}

/// A located fill: when/where the entry order filled, and the candle slice from
/// the fill bar onward (the post-fill SL/TP/break-even search window). Shared by
/// [`simulate_fill`] and [`breakeven_armed_at`] so the two can't disagree on
/// *which* bar the order filled on.
struct Fill<'a> {
    fill_at: chrono::DateTime<chrono::Utc>,
    entry_price: f64,
    rest: &'a [BidAskCandle],
}

/// Phase 1 of the fill: find where the entry order filled. A short entry sells
/// (fills on the bid book); a long entry buys (fills on the ask book). A market
/// entry crosses the spread at once. The recorded fill price is the placed level
/// (the resting order's price), not the book extreme.
///
/// `candles[0]` is the **fire bar** — the bar the enter fired on. A pending
/// order is only placed once that bar has *closed* (the engine decides the fire
/// on the cron tick that processes the closed bar; under `needs_confirmed` the
/// confirmation itself isn't known until this bar's close). So a Stop/Limit order
/// cannot interact with the fire bar's own intrabar path — the earliest bar it
/// can fill on is the **next** one, so we search from `candles[1..]`. A Market
/// entry is the exception: it fills at the fire bar's close (the shell price).
///
/// Returns `None` when the pending order never fills within the window (the
/// caller maps that to `NeverFilled`).
fn find_fill<'a>(
    resolved: &Resolved,
    intent: &Intent,
    shell: &Shell,
    dir: Direction,
    candles: &'a [BidAskCandle],
) -> Option<Fill<'a>> {
    let entry_book = book_for(Leg::Entry, dir);
    match resolved.entry {
        ResolvedEntry::Market { reference_price } => Some(Fill {
            fill_at: shell.time,
            entry_price: reference_price,
            rest: candles,
        }),
        ResolvedEntry::Stop { trigger_price } | ResolvedEntry::Limit { trigger_price } => {
            // Skip the fire bar (index 0): the resting order isn't live until
            // after it closes. `get(1..)` is empty when the fire bar is the only
            // recorded candle, yielding `None` (no later bar to fill on yet).
            let after_fire = candles.get(1..).unwrap_or(&[]);
            // Bar-expiry (`expiry_bars`): the worker cancels a still-resting order
            // `N` bars after the fire bar (its `cancel_at = next_candle_timestamp_N`).
            // Mirror that here by bounding the fill window to the first `N` bars
            // after the fire bar — a cross on a later bar is an order the worker
            // would already have cancelled, so it must not fill. A static
            // `expiry_bars` is honoured; a script-resolved one (Rhai) is beyond
            // this pure price-path sim, so it's treated as "no bar-expiry"
            // (unbounded), same as `None`.
            let expiry_bars = intent
                .expiry_bars
                .as_ref()
                .and_then(|t| t.as_static())
                .copied();
            let fill_window: &[BidAskCandle] = match expiry_bars {
                Some(n) => after_fire
                    .get(..(n as usize).min(after_fire.len()))
                    .unwrap_or(&[]),
                None => after_fire,
            };
            let i = fill_window
                .iter()
                .position(|c| book_crosses(c, entry_book, trigger_price))?;
            // `i` indexes `fill_window` (a prefix of `after_fire`), so the fill
            // bar is `candles[i + 1]`. The post-fill search **includes** the fill
            // bar itself (`candles[i + 1..]`): an order that fills mid-bar can be
            // stopped out (or hit TP) later in that SAME bar.
            Some(Fill {
                fill_at: after_fire[i].time,
                entry_price: trigger_price,
                rest: &candles[i + 1..],
            })
        }
    }
}

/// The bar on which break-even **would arm** for this enter — i.e. the first
/// post-fill candle whose *close* runs past the `breakeven` threshold (50%-to-TP
/// by default), at or before the position's exit. `None` when the enter carries
/// no `breakeven`, never fills, or exits (SL/TP) before any candle arms it.
///
/// This is the **replay stand-in for the live cron amend**: in production
/// [`crate::breakeven_watch`] doesn't move the stop at fill-time — it runs every
/// 15-min cron tick and sends `amend_stop(entry)` to the broker on the first tick
/// that observes a closed candle past the threshold. The bar returned here is
/// exactly that candle, so a replay can show "this is when the worker would have
/// amended the broker SL to break-even." It shares the same arming predicate
/// ([`Breakeven::close_arms`]) and fill-finding ([`find_fill`]) as the fill
/// simulator, so the reported bar can't drift from the simulated outcome.
///
/// Pure and side-effect-free; the report calls it independently of
/// [`simulate_fill`] so the `SimOutcome` enum (and every saved fixture) is
/// untouched.
pub fn breakeven_armed_at(
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
) -> Option<chrono::DateTime<chrono::Utc>> {
    let be = intent.breakeven?;
    let resolved = Resolved::from_intent(intent, shell, pip_size).ok()?;
    let dir = resolved.direction;
    let fill = find_fill(&resolved, intent, shell, dir, candles)?;

    let exit_book = book_for(Leg::Exit, dir);
    let level = be.arms_at(fill.entry_price, resolved.take_profit);
    // Walk the post-fill path exactly as Phase 2 does: an exit (SL/TP) before any
    // arming close means break-even never armed during the position's life.
    for c in fill.rest {
        if book_crosses(c, exit_book, resolved.stop_loss)
            || book_crosses(c, exit_book, resolved.take_profit)
        {
            return None;
        }
        if be.close_arms(dir, level, c.c) {
            return Some(c.time);
        }
    }
    None
}

/// Why the live cron sweep would have cancelled a `NeverFilled` resting order,
/// and the bar timestamp at which it would have acted — the replay stand-in for
/// the worker's [`sweep_pending_orders`](../../../src/cron/sweep.rs).
///
/// `simulate_fill` reports `NeverFilled` for *any* order that never triggered —
/// but the live worker doesn't passively wait: every cron tick it walks each
/// resting `EntryAttempt` and **cancels** it once its alert window expired, its
/// bar-based `cancel_at` passed, it sits inside a market-hours blackout, or
/// current price overtook its stop-loss. A replay that can't tell an order the
/// worker would have *swept* from one that merely never triggered diverges
/// silently from production. This walks the post-fire candle path and returns
/// the first sweep the worker would have made.
///
/// Mirrors the worker's `sweep_one` branch priority at each bar: **expired**
/// (alert window) → **bar-expiry** (`cancel_at`) → *blackout* → **SL-breach**.
/// It reuses the shared `core::sweep_gate` predicates so worker and replay can't
/// drift, and derives `cancel_at` via the same `core::resolve_cancel_at` the
/// worker uses (off the Pine-shipped forward bar-close menu on the shell).
///
/// Returns `None` when no sweep condition is reached within the candle path, or
/// when the order would never rest (a Market entry / an unresolved intent — the
/// caller's `NeverFilled` is then not a swept order). The **blackout** case is
/// deliberately not reconstructed: the per-instrument no-entry windows are
/// daily-cron-written to KV and the offline replay doesn't have them — so a
/// blackout-driven sweep returns `None` here rather than a faked `Blackout`.
/// TODO: surface market-hours blackout once an offline window source lands —
/// REPLAY-PARITY-AUDIT item 3.
///
/// Pure and side-effect-free; the report calls it independently of
/// [`simulate_fill`] so the `SimOutcome` enum (and every saved fixture) stays
/// untouched — the same pattern [`breakeven_armed_at`] uses.
pub fn sweep_reason(
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
) -> Option<(SweepReason, chrono::DateTime<chrono::Utc>)> {
    let resolved = Resolved::from_intent(intent, shell, pip_size).ok()?;

    // A Market entry fills at once (it never rests), so a `NeverFilled` Market is
    // not a swept order — there's nothing for the sweep to cancel.
    let sl = match resolved.entry {
        ResolvedEntry::Stop { .. } | ResolvedEntry::Limit { .. } => resolved.stop_loss,
        ResolvedEntry::Market { .. } => return None,
    };
    let dir = resolved.direction;

    // Derive the bar-based `cancel_at` exactly as the worker's `run_enter` does:
    // off the Pine-shipped forward bar-close menu on the shell, capped at the
    // alert window. A non-static / out-of-range / absent `expiry_bars` yields no
    // bar-expiry (matching the worker, which only sets `cancel_at` when it
    // resolves cleanly).
    let cancel_at = intent
        .expiry_bars
        .as_ref()
        .and_then(|t| t.as_static())
        .copied()
        .and_then(|bars| {
            trade_control_core::intent::resolve_cancel_at(bars, shell, intent.not_after).ok()
        });

    // The order rests from the bar *after* the fire bar (a pending order isn't
    // live until the fire bar closes — same skip `find_fill` applies). Walk those
    // live bars chronologically; the first that trips a sweep branch wins.
    for c in candles.get(1..).unwrap_or(&[]) {
        if intent.not_after < c.time {
            return Some((SweepReason::Expired, c.time));
        }
        if bar_expiry_due(cancel_at, c.time) {
            return Some((SweepReason::BarExpiry, c.time));
        }
        // SL-breach uses the bar's mid close as the "current price" the live
        // sweep would read from `get_current_price` (a mid quote). Intrabar
        // wick noise is intentionally ignored: the sweep samples a point price
        // per tick, not the bar range.
        if breach_detected(dir, c.c, sl) {
            return Some((SweepReason::SlBreached, c.time));
        }
    }

    None
}

/// Which broker book a price level is tested against.
#[derive(Clone, Copy, PartialEq)]
enum Book {
    /// A sell fills here (short entry, long exit).
    Bid,
    /// A buy fills here (long entry, short exit).
    Ask,
}

/// Which leg of the trade a level belongs to — entry (open) vs SL/TP (close).
#[derive(Clone, Copy)]
enum Leg {
    Entry,
    Exit,
}

/// The broker book the given leg fills on: a buy uses the ask, a sell uses the
/// bid. Entry side is the trade direction; exit side is its opposite (you close
/// by trading the other way).
fn book_for(leg: Leg, dir: Direction) -> Book {
    match (leg, dir) {
        (Leg::Entry, Direction::Long) => Book::Ask, // buy to open
        (Leg::Entry, Direction::Short) => Book::Bid, // sell to open
        (Leg::Exit, Direction::Long) => Book::Bid,  // sell to close
        (Leg::Exit, Direction::Short) => Book::Ask, // buy to close
    }
}

/// Whether the candle's chosen book range spans `level` (an exact-level touch).
/// Direction-agnostic on the level itself: a long stop above and a short stop
/// below are both "this book's high–low range reached the level". The *book*
/// (bid vs ask) is what carries the real per-bar spread.
fn book_crosses(c: &BidAskCandle, book: Book, level: f64) -> bool {
    let (lo, hi) = match book {
        Book::Bid => (c.bid_l, c.bid_h),
        Book::Ask => (c.ask_l, c.ask_h),
    };
    lo <= level && level <= hi
}

/// The resolved trade direction — a small public helper so callers can label a
/// fill's side without re-resolving the intent.
pub fn direction_of(resolved: &Resolved) -> Direction {
    resolved.direction
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use trade_control_core::intent::{
        Action, BrokerKind, EntrySpec, PriceAnchor, PriceRef, TakeProfit,
    };
    use trade_control_core::tunable::Tunable;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// A bar with **bid == ask == mid** (zero spread) — the data-source-serves-
    /// mid-only case. Lets the level-logic tests read as plain OHLC while
    /// exercising the bid/ask code paths through the degenerate (zero-spread)
    /// branch.
    fn candle(time: &str, o: f64, h: f64, l: f64, c: f64) -> BidAskCandle {
        BidAskCandle {
            time: ts(time),
            o,
            h,
            l,
            c,
            bid_o: o,
            bid_h: h,
            bid_l: l,
            bid_c: c,
            ask_o: o,
            ask_h: h,
            ask_l: l,
            ask_c: c,
        }
    }

    /// A bar with explicit bid/ask books (mid is their midpoint, unused by the
    /// fill test). For the spread-specific tests: `bid_*` is the sell book,
    /// `ask_*` the buy book.
    #[allow(clippy::too_many_arguments)]
    fn ba_candle(time: &str, bid_h: f64, bid_l: f64, ask_h: f64, ask_l: f64) -> BidAskCandle {
        BidAskCandle {
            time: ts(time),
            o: (bid_l + ask_h) / 2.0,
            h: (bid_h + ask_h) / 2.0,
            l: (bid_l + ask_l) / 2.0,
            c: (bid_l + ask_h) / 2.0,
            bid_o: bid_l,
            bid_h,
            bid_l,
            bid_c: bid_l,
            ask_o: ask_h,
            ask_h,
            ask_l,
            ask_c: ask_h,
        }
    }

    /// A long stop-entry intent with SL `1.1000` and TP `1.1150` as absolute
    /// levels, so the test doesn't depend on anchor resolution. The entry's stop
    /// trigger is overridden per-test.
    fn long_stop_intent() -> Intent {
        let mut i = base_enter();
        i.direction = Some(Direction::Long);
        i.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 0.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        i.stop_loss = Some(PriceRef::Absolute { absolute: 1.1000 });
        i.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.1150,
        }));
        i
    }

    fn base_enter() -> Intent {
        Intent {
            entry_level_vetos: Vec::new(),
            v: 1,
            id: "sim-test".into(),
            not_before: None,
            not_after: ts("2026-06-30T00:00:00Z"),
            action: Action::Enter,
            instrument: "EUR_USD".into(),
            direction: None,
            entry: None,
            stop_loss: None,
            take_profit: None,
            risk_pct: Tunable::Static(1.0),
            risk_amount: None,
            size_units: None,
            dry_run: None,
            cooldown_hours: None,
            min_r: None,
            broker: BrokerKind::Oanda,
            account: None,
            step: None,
            name: None,
            ttl_hours: Tunable::Static(0),
            level: None,
            requires_preps: Vec::new(),
            vetos: Vec::new(),
            clears: Vec::new(),
            trade_id: Some("sim-test".into()),
            max_retries: Tunable::Static(0),
            expiry_bars: None,
            allow_entry: None,
            allow_close: None,
            needs_golden: false,
            needs_confirmed: false,
            blackout_id: None,
            news_id: None,
            require_news_window: None,
            require_price_in_ranges: None,
            inside_window: Vec::new(),
            sr_bands: Vec::new(),
            veto_on_reversal: false,
            reason: None,
            mw: None,
            pip_size: None,
            trade_plan: None,
            blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
            breakeven: None,
            include_archived: false,
        }
    }

    /// The shell the fire carried: a trigger candle closing at 1.1040 (below the
    /// 1.1050 stop, so the stop is valid — long stop sits above close).
    fn trigger_shell() -> Shell {
        Shell::from_candle(&candle("2026-06-17T10:00:00Z", 1.1035, 1.1045, 1.1030, 1.1040).mid())
    }

    /// The **fire bar** — `candles[0]` in production, the bar the enter fired on.
    /// A pending order isn't live until this bar closes, so `simulate_fill` skips
    /// it for the fill search. Tests prepend this so the path matches production's
    /// `fire.forward` shape (fire bar first, then the post-fire path). Its range
    /// (1.1041–1.1045) deliberately misses every per-test trigger/SL/TP so its
    /// only role is to be skipped.
    fn fire_bar() -> BidAskCandle {
        candle("2026-06-17T10:30:00Z", 1.1042, 1.1045, 1.1041, 1.1043)
    }

    #[test]
    fn stop_entry_fills_then_takes_profit() {
        let intent = long_stop_intent();
        let shell = trigger_shell();
        // Stop trigger = close (1.1040) + 0 pips → wait, EntrySpec::Stop anchors
        // to `from`=Close=1.1040; long stop must be ABOVE close, so resolver
        // requires trigger > close. With offset 0 that's invalid; use a candle
        // path where the fill candle reaches the resolved trigger.
        // Resolver: trigger = anchor(Close)=1.1040; long-stop-<=close errors.
        // So this intent resolves with InvalidGeometry — assert that path first.
        let outcome = simulate_fill(&intent, &shell, 0.0001, &[]);
        assert!(
            matches!(outcome, SimOutcome::Unresolved(_)),
            "0-offset long stop on close is invalid geometry: {outcome:?}"
        );
    }

    #[test]
    fn offset_stop_fills_then_tp_then_sl_paths() {
        // Stop 10 pips above close → trigger 1.1050. SL 1.1000, TP 1.1150.
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();

        // Path A: a candle reaches 1.1050 (fills), a later one reaches 1.1150 (TP).
        // Each path leads with the fire bar (skipped — order isn't live until it
        // closes), so the first *fillable* bar is index 1.
        let tp_path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills @1.1050
            candle("2026-06-17T12:00:00Z", 1.1052, 1.1160, 1.1050, 1.1155), // hits TP 1.1150
        ];
        match simulate_fill(&intent, &shell, 0.0001, &tp_path) {
            SimOutcome::TookProfit {
                entry_price,
                exit_price,
                ..
            } => {
                assert!((entry_price - 1.1050).abs() < 1e-9);
                assert!((exit_price - 1.1150).abs() < 1e-9);
            }
            other => panic!("expected TookProfit, got {other:?}"),
        }

        // Path B: fills, then a candle reaches the SL 1.1000.
        let sl_path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills
            candle("2026-06-17T12:00:00Z", 1.1050, 1.1051, 1.0995, 1.1000), // hits SL 1.1000
        ];
        match simulate_fill(&intent, &shell, 0.0001, &sl_path) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!((exit_price - 1.1000).abs() < 1e-9);
            }
            other => panic!("expected StoppedOut, got {other:?}"),
        }

        // Path C: never reaches the trigger → NeverFilled.
        let no_fill = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043),
        ];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &no_fill),
            SimOutcome::NeverFilled
        );

        // Path D: fills but neither level touched → FilledOpen.
        let still_open = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills
            candle("2026-06-17T12:00:00Z", 1.1052, 1.1060, 1.1048, 1.1055), // neither
        ];
        assert!(matches!(
            simulate_fill(&intent, &shell, 0.0001, &still_open),
            SimOutcome::FilledOpen { .. }
        ));
    }

    #[test]
    fn entry_level_veto_flips_loss_to_no_fill() {
        // Bug #12 regression — the −110.53 GBP path. Same candles, same
        // resolved entry: with NO entry-level veto the order fills and runs to
        // its stop (a loss); with a breached pcl-exhausted level baked on, the
        // simulator declines before filling (£0), exactly as the worker's
        // `run_enter` gate would.
        use trade_control_core::intent::{EntryLevelVeto, VetoSide};
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();
        // Fills @1.1050, then a candle reaches SL 1.1000 → StoppedOut. Lead with
        // the fire bar (skipped) so the fill lands on the realistic index 1.
        let sl_path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052),
            candle("2026-06-17T12:00:00Z", 1.1050, 1.1051, 1.0995, 1.1000),
        ];

        // Baseline: no veto → the loss path.
        assert!(matches!(
            simulate_fill(&intent, &shell, 0.0001, &sl_path),
            SimOutcome::StoppedOut { .. }
        ));

        // With a pcl-exhausted level the entry (1.1050) is already past
        // (`Above` 1.1040 for a long) → declined, no fill.
        intent.entry_level_vetos = vec![EntryLevelVeto {
            name: "too-high".into(),
            level: 1.1040,
            past: VetoSide::Above,
        }];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &sl_path),
            SimOutcome::Declined {
                name: "too-high".into()
            }
        );

        // An entry short of the level still fills (don't-over-decline control).
        intent.entry_level_vetos = vec![EntryLevelVeto {
            name: "too-high".into(),
            level: 1.1060, // entry 1.1050 < 1.1060 → not past
            past: VetoSide::Above,
        }];
        assert!(matches!(
            simulate_fill(&intent, &shell, 0.0001, &sl_path),
            SimOutcome::StoppedOut { .. }
        ));
    }

    /// Override a short stop-entry intent's entry trigger / SL / TP to absolute
    /// levels, so a BE test controls the exact 50%-to-TP geometry.
    fn i_set_levels(intent: &mut Intent, entry: f64, sl: f64, tp: f64) {
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 0.0,
            offset_atr_pct: None,
            at: Some(entry),
            recover_entry: None,
        });
        intent.stop_loss = Some(PriceRef::Absolute { absolute: sl });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute { absolute: tp }));
    }

    #[test]
    fn breakeven_scratches_a_leg_that_runs_50pct_then_reverses() {
        // Trade-075 Wheat leg-2 shape (BUG-replay-no-breakeven-stop-at-50pct),
        // simplified to round levels but the same geometry: a SHORT that runs
        // past the 50%-to-TP mark on a close, then bounces back to the original
        // SL. Without BE → StoppedOut at the original SL (−1R). With BE → the
        // stop is moved to entry once a candle closes past 50%, so the bounce
        // closes it at break-even (entry), a 0R scratch.
        use trade_control_core::intent::Breakeven;

        // Short stop-entry at 1.1000, original SL 1.1040 (above), TP 1.0900
        // (below). 50%-to-TP level = 1.1000 + 0.5×(1.0900 − 1.1000) = 1.0950.
        let mut intent = short_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.1040, 1.0900);
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );

        // Path: fire bar (skipped) → fill bar reaches the 1.1000 sell-stop on
        // the bid → a candle that CLOSES at 1.0940 (past the 1.0950 BE level,
        // arming BE) → a candle that bounces back up to the 1.1040 ORIGINAL SL.
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1005, 1.1005, 1.0995, 1.1000);
        let runs_past_50 = candle("2026-06-17T12:00:00Z", 1.0990, 1.0992, 1.0935, 1.0940); // closes past 1.0950
        let bounce_to_orig_sl = candle("2026-06-17T13:00:00Z", 1.0945, 1.1041, 1.0944, 1.1000);
        let path = [fire_bar(), fill_bar, runs_past_50, bounce_to_orig_sl];

        // Baseline: NO breakeven → the bounce hits the original 1.1040 SL.
        intent.breakeven = None;
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!(
                    (exit_price - 1.1040).abs() < 1e-9,
                    "no-BE: stopped at the original SL 1.1040, got {exit_price}"
                );
            }
            other => panic!("no-BE: expected StoppedOut at original SL, got {other:?}"),
        }

        // With BE at 50%: the 1.0940 close arms BE (SL → entry 1.1000); the
        // bounce bar (which spans 1.0944..1.1041) now hits the MOVED stop at the
        // entry price 1.1000 first → break-even scratch, not −1R.
        intent.breakeven = Some(Breakeven::at_half());
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut {
                exit_price,
                entry_price,
                ..
            } => {
                assert!(
                    (exit_price - 1.1000).abs() < 1e-9,
                    "BE: stop moved to entry 1.1000, got {exit_price}"
                );
                assert!(
                    (exit_price - entry_price).abs() < 1e-9,
                    "BE: exit == entry → 0R scratch"
                );
            }
            other => panic!("BE: expected break-even stop-out at entry, got {other:?}"),
        }
    }

    /// `breakeven_armed_at` reports the **bar whose close arms break-even** —
    /// the replay stand-in for the live cron amend. Same trade-075 leg-2 geometry
    /// as `breakeven_scratches_a_leg_that_runs_50pct_then_reverses`: the
    /// `runs_past_50` bar (close 1.0940 past the 1.0950 BE level) is the arming
    /// bar, and it must match the bar that the fill sim moves the stop on.
    #[test]
    fn breakeven_armed_at_reports_the_arming_bar() {
        use trade_control_core::intent::Breakeven;
        let mut intent = short_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.1040, 1.0900); // BE level 1.0950
        intent.breakeven = Some(Breakeven::at_half());
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1005, 1.1005, 1.0995, 1.1000);
        let runs_past_50 = candle("2026-06-17T12:00:00Z", 1.0990, 1.0992, 1.0935, 1.0940);
        let bounce_to_orig_sl = candle("2026-06-17T13:00:00Z", 1.0945, 1.1041, 1.0944, 1.1000);
        let path = [fire_bar(), fill_bar, runs_past_50, bounce_to_orig_sl];

        let armed = breakeven_armed_at(&intent, &shell, 0.0001, &path);
        assert_eq!(
            armed,
            Some(runs_past_50.time),
            "BE arms on the bar whose close (1.0940) runs past the 1.0950 level"
        );
    }

    /// No `breakeven` rule → never armed (the field is `None`).
    #[test]
    fn breakeven_armed_at_is_none_without_a_rule() {
        let mut intent = short_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.1040, 1.0900);
        intent.breakeven = None;
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1005, 1.1005, 1.0995, 1.1000);
        let runs_past_50 = candle("2026-06-17T12:00:00Z", 1.0990, 1.0992, 1.0935, 1.0940);
        let path = [fire_bar(), fill_bar, runs_past_50];
        assert_eq!(breakeven_armed_at(&intent, &shell, 0.0001, &path), None);
    }

    /// A position stopped out at the original SL **before** any candle arms BE
    /// reports `None` — break-even never armed during its life.
    #[test]
    fn breakeven_armed_at_is_none_when_stopped_before_arming() {
        use trade_control_core::intent::Breakeven;
        let mut intent = short_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.1040, 1.0900); // BE level 1.0950
        intent.breakeven = Some(Breakeven::at_half());
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );
        // Fill, then a bar that hits the original 1.1040 SL before ever closing
        // past the 1.0950 BE level → BE never arms.
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1005, 1.1005, 1.0995, 1.1000);
        let straight_to_sl = candle("2026-06-17T12:00:00Z", 1.1005, 1.1041, 1.1000, 1.1030);
        let path = [fire_bar(), fill_bar, straight_to_sl];
        assert_eq!(breakeven_armed_at(&intent, &shell, 0.0001, &path), None);
    }

    /// A candle that only WICKS past the 50% level (but closes back short of it)
    /// must NOT arm break-even — the arming basis is the close, not the wick.
    #[test]
    fn breakeven_does_not_arm_on_a_wick() {
        use trade_control_core::intent::Breakeven;
        let mut intent = short_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.1040, 1.0900); // BE level 1.0950
        intent.breakeven = Some(Breakeven::at_half());
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );

        // Fill, then a candle whose LOW (1.0935) wicks past the 1.0950 BE level
        // but whose CLOSE (1.0960) stays short of it → BE must NOT arm. The
        // later bounce to the original SL 1.1040 then takes the full −1R.
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1005, 1.1005, 1.0995, 1.1000);
        let wick_only = candle("2026-06-17T12:00:00Z", 1.0990, 1.0992, 1.0935, 1.0960); // close 1.0960 > level
        let bounce = candle("2026-06-17T13:00:00Z", 1.0965, 1.1041, 1.0960, 1.1000);
        let path = [fire_bar(), fill_bar, wick_only, bounce];

        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!(
                    (exit_price - 1.1040).abs() < 1e-9,
                    "a wick must not arm BE; stop stays at original 1.1040, got {exit_price}"
                );
            }
            other => panic!("expected StoppedOut at original SL (no BE arm), got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_candle_resolves_to_stop() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();
        // One candle fills AND spans both SL and TP → pessimistic: StoppedOut.
        let both = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills @1.1050
            candle("2026-06-17T12:00:00Z", 1.1050, 1.1160, 1.0995, 1.1100), // spans SL & TP
        ];
        assert!(matches!(
            simulate_fill(&intent, &shell, 0.0001, &both),
            SimOutcome::StoppedOut { .. }
        ));
    }

    /// A short stop-entry: absolute trigger 1.1000 (below the shell close so the
    /// sell-stop is validly placed), SL 1.1030 (above), TP 1.0950 (below). Entry
    /// fills on the **bid** book (a sell); the SL closes by *buying*, so it fills
    /// on the **ask** book. The spread-specific test exploits that asymmetry: a
    /// bar whose *mid* high misses the SL but whose *ask* high reaches it stops
    /// out, because the broker closes the short on the ask.
    fn short_stop_intent() -> Intent {
        let mut i = base_enter();
        i.direction = Some(Direction::Short);
        // Absolute trigger — skips the wrong-side guard, so the test controls the
        // exact sell-stop level independent of the shell close.
        i.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 0.0,
            offset_atr_pct: None,
            at: Some(1.1000),
            recover_entry: None,
        });
        i.stop_loss = Some(PriceRef::Absolute { absolute: 1.1030 });
        i.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.0950,
        }));
        i
    }

    #[test]
    fn short_sl_triggers_on_the_ask_book() {
        let intent = short_stop_intent();
        let shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1010, 1.1012, 1.0998, 1.1005).mid(),
        );

        // Fill bar: the short fills on the BID — bid range must reach the 1.1000
        // trigger. Then the SL bar's MID high (1.1029) is a pip short of the
        // 1.1030 SL, but its ASK high (1.1031) reaches it. Since a short closes by
        // buying on the ask, the SL fires — a stop the mid-only view would miss.
        let sl_bar = ba_candle(
            "2026-06-17T10:30:00Z",
            1.1027, // bid_h (mid-ish high; would miss SL on the bid book)
            1.1008, // bid_l
            1.1031, // ask_h — reaches the 1.1030 SL
            1.1012, // ask_l
        );
        let fill_bar = ba_candle(
            "2026-06-17T10:15:00Z",
            1.1000, // bid_h reaches the 1.1000 sell-stop trigger
            1.0996, // bid_l
            1.1004, // ask_h
            1.1000, // ask_l
        );
        let path = [fire_bar(), fill_bar, sl_bar];

        // The ask high (1.1031) reaches the 1.1030 SL → stopped out on the ask.
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut {
                exit_price,
                entry_price,
                ..
            } => {
                // Recorded exit is the placed SL level (the resting order's price).
                assert!((exit_price - 1.1030).abs() < 1e-9);
                // Recorded entry is the placed sell-stop level (1.1000).
                assert!((entry_price - 1.1000).abs() < 1e-9);
            }
            other => panic!("expected ask-side stop-out, got {other:?}"),
        }

        // Control: if the SL bar's ASK high stops a pip *short* of the SL
        // (ask_h 1.1029 < 1.1030), the short stays open — the mid/bid reaching
        // 1.1029 must NOT trigger a close that only the ask can make.
        let near_miss = ba_candle("2026-06-17T10:45:00Z", 1.1027, 1.1008, 1.1029, 1.1012);
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &[fire_bar(), fill_bar, near_miss]),
                SimOutcome::FilledOpen { .. }
            ),
            "ask high 1.1029 misses the 1.1030 SL → still open"
        );
    }

    /// Replay/worker parity: `simulate_fill` resolves the entry via the same
    /// pure `Resolved::from_intent` the worker uses, so an **ATR-pct-buffered**
    /// enter resolves to the identical trigger on both paths. Here a short
    /// stop-entry anchored to `signal_low` with `offset_atr_pct` fills at the
    /// ATR-buffered level, proving the simulator honours the new buffer (no
    /// replay-vs-worker drift — the whole reason resolution lives in core).
    #[test]
    fn atr_buffered_short_stop_fills_at_buffered_trigger() {
        let atr = 0.0040;
        let pct = 0.5;
        // signal_low 1.1000; buffer = 0.5/100 * 0.0040 = 0.00002; short entry
        // anchors to signal_low and pushes DOWN → trigger 1.1000 - 0.00002.
        let buffered_trigger = 1.1000 - (pct / 100.0) * atr;

        let mut intent = short_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::SignalLow,
            offset_pips: 0.0,
            offset_atr_pct: Some(pct),
            at: None,
            recover_entry: None,
        });
        // Keep SL/TP absolute so the geometry is self-contained around the
        // buffered trigger (SL above, TP below — short).
        intent.stop_loss = Some(PriceRef::Absolute { absolute: 1.1030 });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.0950,
        }));

        // Shell carries the latched pattern low + ATR; close sits between the
        // trigger and SL so the short stop is correct-side (trigger < close).
        let mut shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1015, 1.1018, 1.1010, 1.1012).mid(),
        );
        shell.signal_low = Some(1.1000);
        shell.signal_high = Some(1.1030);
        shell.atr = Some(atr);

        // A bar whose bid reaches the buffered sell-stop trigger fills the short.
        let fill_bar = ba_candle(
            "2026-06-17T10:15:00Z",
            buffered_trigger, // bid_h reaches the buffered trigger
            buffered_trigger - 0.0010,
            buffered_trigger + 0.0004,
            buffered_trigger,
        );
        match simulate_fill(&intent, &shell, 0.0001, &[fire_bar(), fill_bar]) {
            SimOutcome::FilledOpen { entry_price, .. } => {
                assert!(
                    (entry_price - buffered_trigger).abs() < 1e-9,
                    "filled at {entry_price}, expected ATR-buffered {buffered_trigger}"
                );
            }
            other => panic!("expected fill at the ATR-buffered trigger, got {other:?}"),
        }
    }

    /// The fail-closed half: an ATR-pct enter whose shell carries **no ATR**
    /// (warmup) is `Unresolved` in the simulator too — same reject the worker
    /// gives — rather than silently filling at a zero-buffer level.
    #[test]
    fn atr_buffered_enter_with_no_atr_is_unresolved() {
        let mut intent = short_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::SignalLow,
            offset_pips: 0.0,
            offset_atr_pct: Some(0.5),
            at: None,
            recover_entry: None,
        });
        let mut shell = Shell::from_candle(
            &candle("2026-06-17T10:00:00Z", 1.1015, 1.1018, 1.1010, 1.1012).mid(),
        );
        shell.signal_low = Some(1.1000);
        shell.atr = None; // warmup / short feed
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &[fire_bar()]),
                SimOutcome::Unresolved(_)
            ),
            "no-ATR ATR-pct enter must be Unresolved, not a zero-buffer fill"
        );
    }

    #[test]
    fn pending_entry_does_not_fill_on_the_fire_bar() {
        // The off-by-one regression (CAD/JPY 21-May confirmed short): the enter
        // fires on the confirming bar, but a resting Stop/Limit order isn't live
        // until that bar *closes*. So even if the fire bar's own range crosses the
        // trigger, the fill must wait for the NEXT bar. A live worker places the
        // order on the cron tick after the bar closes — it cannot fill the bar
        // whose close produced the confirmation.
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();

        // Fire bar already trades through the 1.1050 trigger. The OLD code filled
        // here (index 0); the fix skips it.
        let fire_crosses = candle("2026-06-17T10:30:00Z", 1.1045, 1.1060, 1.1044, 1.1052);

        // Case 1: ONLY the fire bar crosses; nothing after → NeverFilled (not a
        // fill on the fire bar).
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &[fire_crosses]),
            SimOutcome::NeverFilled,
            "a trigger-crossing fire bar must not fill — order not live until it closes"
        );

        // Case 2: fire bar crosses AND a later bar also reaches the trigger → the
        // fill is recorded on the LATER bar (11:00), never the fire bar (10:30).
        let path = [
            fire_crosses,
            candle("2026-06-17T11:00:00Z", 1.1048, 1.1055, 1.1047, 1.1052), // fills here
            candle("2026-06-17T12:00:00Z", 1.1052, 1.1160, 1.1050, 1.1155), // TP 1.1150
        ];
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::TookProfit { fill_at, .. } => {
                assert_eq!(
                    fill_at,
                    ts("2026-06-17T11:00:00Z"),
                    "fill must be the post-fire bar, not the fire bar"
                );
            }
            other => panic!("expected TookProfit filled on the post-fire bar, got {other:?}"),
        }
    }

    #[test]
    fn fill_bar_can_stop_out_same_bar() {
        // A pending order that fills mid-bar can be stopped out later in that
        // SAME bar (a violent breakout: spikes through the entry, then reverses
        // to the SL before the bar closes). The exit search must include the fill
        // bar; with only OHLC the pessimistic tie-break calls a fill-bar that
        // also spans the SL a stop-out.
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050; SL 1.1000, TP 1.1150
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();

        // Bar 1 (post-fire) fills @1.1050 AND its range reaches the SL 1.1000 in
        // the same bar → StoppedOut, exit on this same bar.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1048, 1.1055, 1.0995, 1.1010), // fill + SL
        ];
        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::StoppedOut {
                fill_at, exit_at, ..
            } => {
                assert_eq!(fill_at, ts("2026-06-17T11:00:00Z"));
                assert_eq!(
                    exit_at,
                    ts("2026-06-17T11:00:00Z"),
                    "the stop-out lands on the fill bar itself"
                );
            }
            other => panic!("expected same-bar StoppedOut, got {other:?}"),
        }

        // Control: a fill bar that fills but does NOT reach SL/TP stays open.
        let still_open = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1048, 1.1055, 1.1047, 1.1052), // fill only
        ];
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &still_open),
                SimOutcome::FilledOpen { .. }
            ),
            "a fill bar that doesn't reach SL/TP must stay open, not exit"
        );
    }

    #[test]
    fn bar_expiry_cancels_a_late_fill() {
        // `expiry_bars = 2`: the order is live only for the 2 bars after the fire
        // bar. A trigger cross on the 3rd bar (or later) is an order the worker
        // would already have cancelled → NeverFilled.
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.expiry_bars = Some(Tunable::Static(2));
        let shell = trigger_shell();

        let no_cross = |t: &str| candle(t, 1.1041, 1.1045, 1.1038, 1.1043);

        // Cross only on bar 3 after the fire bar (index 3 of the path) → expired.
        let late = [
            fire_bar(),                                                     // bar 0 (fire)
            no_cross("2026-06-17T11:00:00Z"),                               // bar 1 (live)
            no_cross("2026-06-17T12:00:00Z"),                               // bar 2 (live, last)
            candle("2026-06-17T13:00:00Z", 1.1048, 1.1055, 1.1047, 1.1052), // bar 3 — too late
        ];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &late),
            SimOutcome::NeverFilled,
            "a cross after expiry_bars must not fill — order already cancelled"
        );

        // Cross on bar 2 (the last live bar) → still fills.
        let in_time = [
            fire_bar(),
            no_cross("2026-06-17T11:00:00Z"),
            candle("2026-06-17T12:00:00Z", 1.1048, 1.1055, 1.1047, 1.1052), // bar 2 — fills
        ];
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &in_time),
                SimOutcome::FilledOpen { .. }
            ),
            "a cross on the last live bar still fills"
        );
    }

    // --- sweep_reason -------------------------------------------------------

    /// A never-triggered LONG stop-entry whose price falls past its SL while the
    /// order is still resting → the live cron sweep would cancel it for an
    /// SL-breach. `sweep_reason` reports that, at the breaching bar.
    #[test]
    fn sweep_reason_reports_sl_breach() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050; SL 1.1000
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();

        // Fire bar (skipped), then two bars that never reach the 1.1050 trigger;
        // the second CLOSES at 1.0995 — below the 1.1000 SL → breach.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1040), // rests, no breach
            candle("2026-06-17T12:00:00Z", 1.1010, 1.1012, 1.0990, 1.0995), // close past SL
        ];
        // Sanity: this path is NeverFilled (trigger never reached).
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::NeverFilled
        );
        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &path),
            Some((SweepReason::SlBreached, ts("2026-06-17T12:00:00Z")))
        );
    }

    /// A never-triggered stop-entry whose bar-based `cancel_at` (off the shell's
    /// Pine forward-bar-close menu) passes → swept for bar-expiry.
    #[test]
    fn sweep_reason_reports_bar_expiry() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.expiry_bars = Some(Tunable::Static(1));
        // cancel_at resolves off slot-1 of the shell's forward menu.
        let mut shell = trigger_shell();
        shell.next_candle_timestamp_1 = Some(ts("2026-06-17T11:30:00Z"));

        // Bars never reach the trigger and never breach the SL — only bar-expiry
        // can fire. The 12:00 bar is past the 11:30 cancel_at.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043), // before cancel_at
            candle("2026-06-17T12:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043), // past cancel_at
        ];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::NeverFilled
        );
        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &path),
            Some((SweepReason::BarExpiry, ts("2026-06-17T12:00:00Z")))
        );
    }

    /// A never-triggered stop-entry whose alert window (`not_after`) closes
    /// during the candle path → swept as alert-window expired. Expiry takes
    /// priority over a same-bar SL-breach (worker `sweep_one` branch order).
    #[test]
    fn sweep_reason_reports_alert_window_expiry_first() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050; SL 1.1000
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.not_after = ts("2026-06-17T11:30:00Z");
        let shell = trigger_shell();

        // The 12:00 bar is past not_after AND closes past the SL — expiry wins.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043), // within window
            candle("2026-06-17T12:00:00Z", 1.1010, 1.1012, 1.0990, 1.0995), // past window + SL
        ];
        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &path),
            Some((SweepReason::Expired, ts("2026-06-17T12:00:00Z")))
        );
    }

    /// No sweep condition reached within the path → `None`. A resting order that
    /// simply never triggered (and never breached / expired) is not swept.
    #[test]
    fn sweep_reason_is_none_when_nothing_sweeps() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050; SL 1.1000
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        // No bar-expiry, generous alert window.
        intent.expiry_bars = None;
        let shell = trigger_shell();

        // Bars stay between SL and trigger — never fill, never breach, in-window.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043),
            candle("2026-06-17T12:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043),
        ];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::NeverFilled
        );
        assert_eq!(sweep_reason(&intent, &shell, 0.0001, &path), None);
    }

    /// A Market entry never rests, so a (degenerate) Market `NeverFilled` is not
    /// a swept order → `None`.
    #[test]
    fn sweep_reason_is_none_for_market_entry() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Market);
        let shell = trigger_shell();
        assert_eq!(sweep_reason(&intent, &shell, 0.0001, &[fire_bar()]), None);
    }
}
