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
use trade_control_core::intent::{
    Direction, Intent, Resolved, ResolvedEntry, Shell, SlWiden, widen_sl_to_spread_floor,
};
use trade_control_core::sweep_gate::{
    SweepReason, bar_expiry_due, breach_detected, market_blackout_due,
};

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
    /// The worker's spread-blackout gate (System 1) would have rejected the
    /// entry: the **fire bar**'s `ask − bid` spread (in pips) exceeds the
    /// instrument's elevated threshold while the NY-close-edge blackout
    /// window is open. No order placed — the live worker 423s here.
    /// `simulate_fill` short-circuits before any fill, mirroring `run_enter`.
    ///
    /// **Modelling note (the offline `window_open` stand-in):** the live
    /// gate only samples the spread when the KV `spread-blackout:window`
    /// marker is set, which the daily cron writes at the NY-close edge
    /// (`src/cron/blackout_apply.rs`). The replay has no KV, so it
    /// approximates the marker with
    /// [`trade_control_core::ny_clock::is_ny_close_edge`] on the fire bar's
    /// open time. This is an *approximation*: the real window can persist
    /// past the close hour until the recovery watcher clears it, whereas the
    /// offline stand-in is exactly the close-edge hour. See [`simulate_fill`].
    SpreadBlackout {
        spread_pips: f64,
        threshold_pips: f64,
    },
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
    // `mut` so the SL-spread-floor widen mirror below can move `stop_loss` in
    // place — exactly as the worker's `run_enter` does — before the fill / exit
    // simulation reads it.
    let mut resolved = match Resolved::from_intent(intent, shell, pip_size) {
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

    // Spread-blackout (System 1) — the worker's `run_enter` rejects a
    // brand-new entry that fires during the post-NY-close liquidity trough
    // when the live spread on the instrument is elevated. `simulate_fill`
    // doesn't run `run_enter`, so mirror that gate here from the recorded
    // book: the fire bar's `ask_c − bid_c` IS the spread the worker would
    // have sampled. The decision + per-instrument threshold are shared with
    // the worker (`core::spread_blackout`) so the two can't drift.
    //
    // `window_open` stand-in: the live gate only samples when the KV
    // `spread-blackout:window` marker is set (written by the daily cron at
    // the NY-close edge). Offline there is no KV, so we approximate it with
    // `is_ny_close_edge` on the fire bar's open time — see the `SpreadBlackout`
    // variant doc for why this is an approximation, not an exact mirror.
    if let Some(reject) = spread_blackout_reject(intent, shell, pip_size, candles) {
        return reject;
    }

    // SL-vs-spread floor SALVAGE (mirror of `run_enter`'s widen-then-reject):
    // when the stop is closer than `10 × spread` to entry the worker widens it
    // to `10 × spread` and re-checks R, entering with the wider stop if it still
    // clears `min_r` and declining otherwise. `simulate_fill` doesn't run
    // `run_enter`, so mirror that here off the recorded book — the fire bar's
    // `ask_c − bid_c` IS the spread the worker quoted (same source the
    // spread-blackout mirror uses). Without this the simulator would fill the
    // entry (the worker placed it at the widened SL) but then check the *old*,
    // un-widened stop, stopping the leg out at a level the live broker stop was
    // never at. The widen mutates `resolved.stop_loss` so both `find_fill` and
    // the exit loop below see the widened level — and break-even arms off the
    // widened geometry, matching the live position.
    if let EntryFloor::Rejected = apply_entry_spread_floor(&mut resolved, pip_size, candles) {
        return SimOutcome::Declined {
            name: "sl-widen-below-min-r".to_string(),
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
        let hit_sl = book_reaches(c, exit_book, active_stop, stop_approach(dir));
        let hit_tp = book_reaches(c, exit_book, resolved.take_profit, tp_approach(dir));
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

/// The replay's spread-blackout gate: `Some(SimOutcome::SpreadBlackout { .. })`
/// when the worker's System-1 gate would reject this enter, `None` otherwise.
///
/// The fire bar is `candles[0]` (the bar the enter fired on — the same bar the
/// `shell` was folded from). Its `ask_c − bid_c` over `pip_size` is the spread
/// the live worker would have sampled from a broker quote. A mid-only feed has
/// `bid_c == ask_c` → zero spread → never blacks out (correct: we don't fabricate
/// a spread the data doesn't carry). `window_open` is the NY-close-edge stand-in
/// keyed on the fire bar's open time. Decision + threshold come from
/// `core::spread_blackout`, shared with the worker.
fn spread_blackout_reject(
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
) -> Option<SimOutcome> {
    if pip_size <= 0.0 {
        return None;
    }
    // The fire bar carries the spread the worker would have quoted. Prefer the
    // recorded `candles[0]` (the literal fire bar); fall back to the shell's
    // own time for the window check if the path is empty.
    let fire = candles.first()?;
    let spread_pips = (fire.ask_c - fire.bid_c) / pip_size;
    // Defensive: an exactly-mid bar (bid == ask) yields zero, a malformed book
    // (ask < bid) a negative, and a NaN book a non-finite — none is a blackout.
    // Only a finite positive spread can be sampled.
    if !spread_pips.is_finite() || spread_pips <= 0.0 {
        return None;
    }
    let window_open = trade_control_core::ny_clock::is_ny_close_edge(shell.time);
    let threshold_pips =
        trade_control_core::spread_blackout::elevated_threshold_pips(&intent.instrument);
    trade_control_core::spread_blackout::spread_blackout_decision(
        window_open,
        spread_pips,
        threshold_pips,
    )
    .then_some(SimOutcome::SpreadBlackout {
        spread_pips,
        threshold_pips,
    })
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
            // The side price must approach the trigger from for the order to
            // fill. A *stop* sits on the far side of the market in the trade's
            // direction (long-stop above → price rises into it → `FromBelow`;
            // short-stop below → price falls into it → `FromAbove`). A *limit*
            // is the mirror (long-limit below → `FromAbove`). Using the
            // directional `book_reaches` (not the old bracket test) is what lets
            // a bar that *gaps through* the trigger fill — the bug this fixes.
            let entry_approach = match (&resolved.entry, dir) {
                (ResolvedEntry::Stop { .. }, Direction::Long)
                | (ResolvedEntry::Limit { .. }, Direction::Short) => Approach::FromBelow,
                (ResolvedEntry::Stop { .. }, Direction::Short)
                | (ResolvedEntry::Limit { .. }, Direction::Long) => Approach::FromAbove,
                // Market handled above; unreachable in this arm.
                (ResolvedEntry::Market { .. }, _) => Approach::FromBelow,
            };
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
                .position(|c| book_reaches(c, entry_book, trigger_price, entry_approach))?;
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
        if book_reaches(c, exit_book, resolved.stop_loss, stop_approach(dir))
            || book_reaches(c, exit_book, resolved.take_profit, tp_approach(dir))
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
/// (alert window) → **bar-expiry** (`cancel_at`) → **blackout** (market-hours)
/// → **SL-breach**. It reuses the shared `core::sweep_gate` predicates so worker
/// and replay can't drift, and derives `cancel_at` via the same
/// `core::resolve_cancel_at` the worker uses (off the Pine-shipped forward
/// bar-close menu on the shell).
///
/// `blackout_windows` are the instrument's market-hours no-entry windows. Live
/// they're written to KV daily by the `blackout_hours` cron from TradeNation
/// `market_info`; the offline replay fetches the *same* `market_info` at startup
/// and resolves them through the *same* `core::windows_from_session` deriver, so
/// a blackout-driven sweep here matches the live worker's. Pass an **empty**
/// slice when no windows are available (TradeNation unreachable, an OANDA-sourced
/// replay, a 24h market, or an unparseable session) — the blackout branch then
/// never fires, exactly the worker's fail-open, and the order falls through to
/// the SL-breach check / plain "never triggered" verdict.
///
/// Returns `None` when no sweep condition is reached within the candle path, or
/// when the order would never rest (a Market entry / an unresolved intent — the
/// caller's `NeverFilled` is then not a swept order).
///
/// Pure and side-effect-free; the report calls it independently of
/// [`simulate_fill`] so the `SimOutcome` enum (and every saved fixture) stays
/// untouched — the same pattern [`breakeven_armed_at`] uses.
pub fn sweep_reason(
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
    blackout_windows: &[trade_control_core::intent::NoEntryWindow],
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
        // Market-hours blackout: the resting order is caught inside the
        // instrument's daily close→open gap. Runs BEFORE SL-breach to match the
        // worker's `sweep_one` ordering — across a closed session a price-based
        // check would read a stale quote, so the closed market itself is the
        // trigger. Empty `blackout_windows` ⇒ `market_blackout_due` is false ⇒
        // fail-open (no fabricated blackout), same as the worker's reject gate.
        if market_blackout_due(blackout_windows, c.time) {
            return Some((SweepReason::Blackout, c.time));
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

/// Outcome of the System-1 entry SL-spread floor applied to a resolved bracket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EntryFloor {
    /// The floor left the stop where it was (already ≥ 10× spread), or moved it
    /// to `10 × spread`. `spread_pips` is the fire-bar spread the floor used —
    /// surfaced so the journal can show *which* spread sized the placed stop.
    Applied { spread_pips: f64 },
    /// Widening to `10 × spread` would drop R below `min_r` — the live worker
    /// declines the entry, so the sim/replay must too.
    Rejected,
}

/// Apply the System-1 entry SL-spread floor to `resolved` **in place**, off the
/// fire bar (`candles.first()`) — the single source of the placed stop.
///
/// This is the mirror of `run_enter`'s widen-then-reject: when the signed stop
/// sits closer than `10 × spread` to entry, the worker widens it to `10 × spread`
/// (entering with the wider stop if R still clears `min_r`, declining otherwise).
/// Both `simulate_fill` (the exit sim) and `widened_stop_at` (the System-2
/// baseline) call this so the placed stop, the simulated exit, and the System-2
/// "from" level are all the **same** floored number — they can't drift into the
/// three-different-SLs confusion the journal showed on EUR/AUD
/// `hs-eur-aud-3d0b5dda`. Returns the spread used (for display) or a reject.
///
/// No fire bar (empty path) ⇒ `Applied { spread_pips: 0.0 }` — nothing to floor.
pub fn apply_entry_spread_floor(
    resolved: &mut Resolved,
    pip_size: f64,
    candles: &[BidAskCandle],
) -> EntryFloor {
    let Some(fire) = candles.first() else {
        return EntryFloor::Applied { spread_pips: 0.0 };
    };
    let spread_price = fire.ask_c - fire.bid_c;
    match widen_sl_to_spread_floor(
        resolved.entry.reference_price(),
        resolved.stop_loss,
        resolved.take_profit,
        spread_price,
        resolved.min_r,
    ) {
        SlWiden::Unchanged => {}
        SlWiden::Widened { new_stop_loss, .. } => {
            resolved.stop_loss = new_stop_loss;
        }
        SlWiden::Reject { .. } => return EntryFloor::Rejected,
    }
    let spread_pips = if pip_size > 0.0 && pip_size.is_finite() {
        spread_price / pip_size
    } else {
        f64::NAN
    };
    EntryFloor::Applied { spread_pips }
}

/// A System-2 spread-widen the replay reconstructs from the candle path: the
/// bar whose spread tripped the widen, the new (widened) stop level, and — since
/// the widen is *transient* live — the bar at which the recovery watcher would
/// restore the original stop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpreadWiden {
    /// Open-time of the bar whose spread crossed the widen trigger.
    pub at: chrono::DateTime<chrono::Utc>,
    /// The stop the open position actually carried before this widen — i.e. the
    /// resolved stop **after** the System-1 entry spread floor (the same number
    /// the `order:` line and `simulate_fill` place at). System 2 widens from and
    /// restores to THIS level, so the journal's widen/restore lines reconcile
    /// with the placed stop instead of the un-floored signed level.
    pub original_stop: f64,
    /// Pips crossed on the widen bar (`ask_c − bid_c`), for the journal display.
    pub widen_spread_pips: f64,
    /// The stop after widening away from price (the shared
    /// [`trade_control_core::blackout_widen::widened_stop`] result).
    pub widened_stop: f64,
    /// Open-time of the bar at which the live recovery watcher
    /// (`blackout_watch::watch_recovery`) would restore the original stop:
    /// the first post-widen bar whose spread has dropped to/under the recovered
    /// cutoff (4 pips), or — if the spread stays elevated — the 3-hour backstop.
    /// `None` when neither happens before the position exits or the window ends
    /// (the widen would still be active at exit). The widen is a **transient**
    /// shield, not a permanent risk change — this field is what makes the replay
    /// journal say so.
    pub restored_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Reconstruct a System-2 spread-blackout stop widen from the candle path, if
/// one would apply while this enter's position is open.
///
/// In production the live cron (`src/cron/blackout_apply.rs`) samples
/// `ask − bid`, and when it's inside the spread-blackout window it widens the
/// broker stop *away* from price by the live spread (floored/clamped 22–40 pips
/// via [`trade_control_core::blackout_widen::clamp_widen`] /
/// [`trade_control_core::blackout_widen::widened_stop`]). A widened stop changes
/// the **exit price** — without this, a replay would stop the position out at
/// the original (tighter) level and diverge from the live worker.
///
/// **The per-instrument spread-hour gate (2026-07-05).** The live cron widens
/// at each instrument's *own* learned spread hours (baked from the sampler
/// data), not one global NY-close hour — Gold overnight, EUR/USD at 21:00,
/// indices at their own — via
/// [`trade_control_core::spread_blackout::spread_hour_widen_pips`]. This replay
/// mirrors that: a bar qualifies when `spread_hour_widen_pips(instrument,
/// c.time)` is `Some` (in/leading into a learned spread hour → widen by the
/// baked p90), OR — for an **un-sampled** instrument with no learned hours — the
/// bar is at the legacy NY-close edge
/// ([`trade_control_core::ny_clock::is_ny_close_edge`], 21:00 UTC EDT / 22:00
/// EST) AND its live spread reaches `widen_trigger_pips`. The pre-2026-07-05
/// behaviour (global NY-close gate + `clamp_widen`) survives verbatim on the
/// fallback path so un-sampled assets don't regress.
///
/// **The trigger / widen size.** For a baked spread-hour bar the widen is
/// [`trade_control_core::blackout_widen::spread_hour_widen_size`] — baked p90
/// primary, live spread as a floor, per-instrument ceiling (see that fn's
/// docs). For the legacy fallback the caller's `widen_trigger_pips` (the
/// instrument's `baked-baseline × 5` from
/// [`trade_control_core::spread_blackout::elevated_threshold_pips`], the same
/// number System 1 uses) still gates, and the amount is the flat 22–40
/// [`trade_control_core::blackout_widen::clamp_widen`].
///
/// Pure and side-effect-free. Returns `None` when the enter has no fill, exits
/// before any qualifying spread bar, or no NY-close-edge bar's spread reaches
/// the trigger.
pub fn widened_stop_at(
    intent: &Intent,
    shell: &Shell,
    pip_size: f64,
    candles: &[BidAskCandle],
    widen_trigger_pips: f64,
) -> Option<SpreadWiden> {
    if !pip_size.is_finite() || pip_size <= 0.0 {
        return None;
    }
    let mut resolved = Resolved::from_intent(intent, shell, pip_size).ok()?;
    // Floor the baseline to the placed stop (System 1) so System 2 widens from
    // and restores to the SAME level the order line shows — not the un-floored
    // signed SL. A reject means the live worker declined the entry, so there's
    // no open position to widen.
    if let EntryFloor::Rejected = apply_entry_spread_floor(&mut resolved, pip_size, candles) {
        return None;
    }
    let dir = resolved.direction;
    let fill = find_fill(&resolved, intent, shell, dir, candles)?;
    let exit_book = book_for(Leg::Exit, dir);
    let original_stop = resolved.stop_loss;

    for (i, c) in fill.rest.iter().enumerate() {
        // The original stop is still the live one until a widen fires, so an
        // exit (SL/TP) before any qualifying spread bar means no widen applied.
        if book_reaches(c, exit_book, original_stop, stop_approach(dir))
            || book_reaches(c, exit_book, resolved.take_profit, tp_approach(dir))
        {
            return None;
        }
        // Per-instrument spread-hour gate — mirror the live cron's System 2
        // (`widen_open_stops_for_spread_hours`). `spread_hour_widen_pips`
        // returns `Some(baked_p90)` iff this bar's instrument is in (or leading
        // into) one of its learned spread hours; `None` means either "not a
        // spread hour now" or "un-sampled instrument", disambiguated by the
        // legacy `is_ny_close_edge` fallback (so un-sampled assets keep the old
        // NY-close-only behaviour). Before 2026-07-05 this was a single global
        // `is_ny_close_edge` gate for ALL instruments — see
        // `[[strategy_changes_in_both_replayer_and_worker]]`; the worker + this
        // replay must gate identically.
        let baked_p90 =
            trade_control_core::spread_blackout::spread_hour_widen_pips(&intent.instrument, c.time);
        if baked_p90.is_none() && !trade_control_core::ny_clock::is_ny_close_edge(c.time) {
            continue;
        }
        let spread_pips = (c.ask_c - c.bid_c) / pip_size;
        if !spread_pips.is_finite() {
            continue;
        }
        // A baked spread-hour bar widens regardless of the live spread reading
        // (the baked p90 is the primary widen; the timing is what the mask
        // asserts). The legacy fallback still requires the live spread to reach
        // the per-instrument trigger, matching the pre-2026-07-05 behaviour.
        let widen_pips = match baked_p90 {
            Some(p90) => {
                trade_control_core::blackout_widen::spread_hour_widen_size(p90, spread_pips)
            }
            None if spread_pips >= widen_trigger_pips => {
                trade_control_core::blackout_widen::clamp_widen(spread_pips)
            }
            None => continue,
        };
        let widened = trade_control_core::blackout_widen::widened_stop(
            dir,
            original_stop,
            widen_pips,
            pip_size,
        );
        let restored_at = restore_bar(&fill.rest[i + 1..], c.time, pip_size);
        return Some(SpreadWiden {
            at: c.time,
            original_stop,
            widen_spread_pips: spread_pips,
            widened_stop: widened,
            restored_at,
        });
    }
    None
}

/// When the live recovery watcher (`blackout_watch::watch_recovery`) would
/// restore the widened stop, reconstructed from the post-widen candle path.
///
/// Mirrors the two live restore triggers (Safety Rules 1 & 2 in
/// `blackout_watch`): the first bar whose spread has dropped to/under the
/// recovered cutoff (`SPREAD_BLACKOUT_RECOVERED_PIPS`, 4 pips) — clock-agnostic,
/// so recovery is NOT gated on the NY-close edge — or, failing that, the 3-hour
/// backstop (`BLACKOUT_BACKSTOP_SECONDS`), whichever comes first. `bars` are the
/// candles strictly after the widen bar; `widen_at` is the widen bar's open
/// time. `None` when neither trigger lands within the provided path (the widen
/// is still active at the window's end).
fn restore_bar(
    bars: &[BidAskCandle],
    widen_at: chrono::DateTime<chrono::Utc>,
    pip_size: f64,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let recovered_cutoff = trade_control_core::spread_blackout::SPREAD_BLACKOUT_RECOVERED_PIPS;
    let backstop_secs = trade_control_core::spread_blackout::BLACKOUT_BACKSTOP_SECONDS;
    let backstop_at = widen_at + chrono::Duration::seconds(backstop_secs as i64);
    for c in bars {
        // Backstop fires first if this bar is at/after the 3h mark, regardless
        // of spread — the live watcher clears unconditionally there.
        if c.time >= backstop_at {
            return Some(c.time);
        }
        let spread_pips = (c.ask_c - c.bid_c) / pip_size;
        if spread_pips.is_finite() && spread_pips <= recovered_cutoff {
            return Some(c.time);
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

/// The side a **stop-loss** is approached from for the given trade direction: a
/// long's SL sits below (price falls into it → `FromAbove`); a short's sits above
/// (`FromBelow`).
fn stop_approach(dir: Direction) -> Approach {
    match dir {
        Direction::Long => Approach::FromAbove,
        Direction::Short => Approach::FromBelow,
    }
}

/// The side a **take-profit** is approached from — the mirror of the stop: a
/// long's TP is above (price rises into it → `FromBelow`), a short's below.
fn tp_approach(dir: Direction) -> Approach {
    match dir {
        Direction::Long => Approach::FromBelow,
        Direction::Short => Approach::FromAbove,
    }
}

/// Which side price approaches a level from — the load-bearing distinction for a
/// *touch* (a triggered stop/limit, an SL/TP hit) versus mere *containment*.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Approach {
    /// Price rises to the level: touched when the bar's high reaches it.
    /// A long-stop entry, a long's take-profit, a short's stop-loss.
    FromBelow,
    /// Price falls to the level: touched when the bar's low reaches it.
    /// A short-stop entry, a short's take-profit, a long's stop-loss.
    FromAbove,
}

/// Whether the candle's chosen book range **reaches** `level`, approaching from
/// the given side. This is a *directional touch*, not containment: a bar that
/// gaps or opens already past the level (its whole range on the far side) still
/// counts, because price traded through the level to get there.
///
/// `FromBelow` ⇒ `high >= level` (an ascending order/target is hit the moment the
/// high reaches it, even if the low never dips back below). `FromAbove` ⇒
/// `low <= level`. The old bracket test (`lo <= level <= hi`) silently *missed*
/// the gap-through case — an up-gap through a long-stop trigger left every
/// post-fire bar's low above the trigger, so the order was reported NeverFilled
/// even though a real broker stop fills on the gap. The *book* (bid vs ask) still
/// carries the real per-bar spread. See BUG-replay-stop-fill-gap.
fn book_reaches(c: &BidAskCandle, book: Book, level: f64, approach: Approach) -> bool {
    let (lo, hi) = match book {
        Book::Bid => (c.bid_l, c.bid_h),
        Book::Ask => (c.ask_l, c.ask_h),
    };
    match approach {
        Approach::FromBelow => hi >= level,
        Approach::FromAbove => lo <= level,
    }
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

    /// Regression for BUG-replay-stop-fill-gap (AUD/NZD iH&S long 2026-07-06):
    /// price **gapped up through** the long-stop trigger, so every post-fire bar
    /// OPENED already above it — the bar's low never dipped back to the trigger.
    /// The old bracket fill test (`lo <= trigger <= hi`) reported NeverFilled even
    /// though a real broker stop fills on the gap. `book_reaches`/`FromBelow`
    /// (`hi >= trigger`) fills it.
    #[test]
    fn long_stop_fills_when_price_gaps_up_through_trigger() {
        // Long stop 10 pips above close → trigger 1.1050.
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();

        // The fillable bar OPENS at 1.1052 (already past the 1.1050 trigger) and
        // its whole range 1.1051–1.1060 sits ABOVE the trigger — a gap-through.
        // Old bracket test: low 1.1051 > 1.1050 → missed. New: high 1.1060 >=
        // 1.1050 → fills @ the trigger price.
        let gap_up = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1052, 1.1060, 1.1051, 1.1058), // gaps through
            candle("2026-06-17T12:00:00Z", 1.1058, 1.1160, 1.1056, 1.1155), // hits TP 1.1150
        ];
        match simulate_fill(&intent, &shell, 0.0001, &gap_up) {
            SimOutcome::TookProfit { entry_price, .. } => {
                assert!(
                    (entry_price - 1.1050).abs() < 1e-9,
                    "fills at the stop trigger, not the gap-open price"
                );
            }
            other => panic!("gapped-through long stop must fill, got {other:?}"),
        }
    }

    /// Mirror of the above for a **short** stop: price gaps DOWN through the
    /// trigger, every post-fire bar's high stays below it. `FromAbove`
    /// (`lo <= trigger`) fills it; the old bracket test missed it.
    #[test]
    fn short_stop_fills_when_price_gaps_down_through_trigger() {
        // Absolute sell-stop trigger 1.1000, SL 1.1030 (above), TP 1.0950 (below).
        let intent = short_stop_intent();
        let shell = trigger_shell();

        // Fillable bar OPENS at 1.0998 (already below the 1.1000 trigger), whole
        // range 1.0990–1.0999 below it — a down-gap. High 1.0999 < 1.1000 so the
        // old bracket test missed it; low 1.0990 <= 1.1000 fills.
        let gap_down = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.0998, 1.0999, 1.0990, 1.0992), // gaps through
            candle("2026-06-17T12:00:00Z", 1.0992, 1.0994, 1.0945, 1.0948), // hits TP 1.0950
        ];
        match simulate_fill(&intent, &shell, 0.0001, &gap_down) {
            SimOutcome::TookProfit { entry_price, .. } => {
                assert!(
                    (entry_price - 1.1000).abs() < 1e-9,
                    "fills at the short-stop trigger"
                );
            }
            other => panic!("gapped-through short stop must fill, got {other:?}"),
        }
    }

    /// A long whose stop-loss is **gapped through** (bar opens already below the
    /// SL, low and high both under it) must still stop out — the same gap bug on
    /// the exit leg. `stop_approach(Long)` = `FromAbove` (`lo <= sl`) catches it.
    #[test]
    fn long_stops_out_when_price_gaps_down_through_sl() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let shell = trigger_shell();
        // Fills @1.1050, then a bar GAPS DOWN entirely below the SL 1.1000
        // (range 1.0980–1.0990, both under 1.1000). Bracket test would miss it.
        let gap_sl = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1042, 1.1055, 1.1041, 1.1052), // fills @1.1050
            candle("2026-06-17T12:00:00Z", 1.0990, 1.0990, 1.0980, 1.0985), // gaps below SL
        ];
        match simulate_fill(&intent, &shell, 0.0001, &gap_sl) {
            SimOutcome::StoppedOut { exit_price, .. } => {
                assert!((exit_price - 1.1000).abs() < 1e-9);
            }
            other => panic!("gapped-through SL must stop out, got {other:?}"),
        }
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

    /// An NY-close-edge UTC instant — 21:00 UTC on an EDT day (12-Mar-2026,
    /// past the 2nd Sunday of March). `is_ny_close_edge` is true here, so the
    /// offline blackout window stand-in is "open". Used to fire enters inside
    /// the trough.
    const EDGE_TS: &str = "2026-03-12T21:00:00Z";
    /// A non-edge UTC instant — 10:00 UTC, mid-London-session, window closed.
    const NON_EDGE_TS: &str = "2026-03-12T10:00:00Z";

    /// A fire bar with an explicit `ask_c − bid_c` spread, in PRICE units, at
    /// `time`. Only the close books carry the spread (the worker samples a quote
    /// close ≈ the bar close); the rest are filled in arbitrarily but
    /// consistently so the bar is well-formed and never crosses any per-test
    /// level.
    fn spread_fire_bar(time: &str, mid: f64, spread_price: f64) -> BidAskCandle {
        let half = spread_price / 2.0;
        BidAskCandle {
            time: ts(time),
            o: mid,
            h: mid + 0.0001,
            l: mid - 0.0001,
            c: mid,
            bid_o: mid - half,
            bid_h: mid - half + 0.0001,
            bid_l: mid - half - 0.0001,
            bid_c: mid - half,
            ask_o: mid + half,
            ask_h: mid + half + 0.0001,
            ask_l: mid + half - 0.0001,
            ask_c: mid + half,
        }
    }

    /// A resolvable long stop-entry (trigger 10 pips above the 1.1040 close, so
    /// the geometry is valid and resolution doesn't short-circuit before the
    /// spread gate). The spread tests vary only the fire bar's book + time.
    fn resolvable_long_stop() -> Intent {
        let mut i = long_stop_intent();
        i.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050 > close 1.1040 → valid long stop
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        i
    }

    #[test]
    fn elevated_spread_inside_ny_close_edge_is_blacked_out() {
        // EUR_USD isn't in the baked baseline (keyed by the TN name "EUR/USD"),
        // so the flat 8-pip fallback applies. A fire bar with a 30-pip spread
        // (0.0030 at pip 0.0001) inside the NY-close-edge window → the worker's
        // System-1 gate would 423, and the replay now mirrors it.
        let intent = resolvable_long_stop();
        // Shell time IS the fire bar time — both at the close edge.
        let shell = Shell::from_candle(&spread_fire_bar(EDGE_TS, 1.1040, 0.0030).mid());
        let path = [spread_fire_bar(EDGE_TS, 1.1040, 0.0030)];

        match simulate_fill(&intent, &shell, 0.0001, &path) {
            SimOutcome::SpreadBlackout {
                spread_pips,
                threshold_pips,
            } => {
                assert!(
                    (spread_pips - 30.0).abs() < 1e-6,
                    "30p spread, got {spread_pips}"
                );
                assert!(
                    (threshold_pips - 8.0).abs() < 1e-9,
                    "flat 8p fallback threshold, got {threshold_pips}"
                );
            }
            other => panic!("expected SpreadBlackout, got {other:?}"),
        }
    }

    #[test]
    fn normal_spread_inside_window_fills() {
        // Control: same NY-close-edge window, but a tight 2-pip spread is below
        // the 8-pip threshold → not blacked out. The order then resolves and
        // (with no fill bar after the fire bar) reports NeverFilled — i.e. the
        // blackout gate let it through.
        let intent = resolvable_long_stop();
        let shell = Shell::from_candle(&spread_fire_bar(EDGE_TS, 1.1040, 0.0002).mid());
        let path = [spread_fire_bar(EDGE_TS, 1.1040, 0.0002)];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::NeverFilled,
            "a tight spread inside the window must pass the blackout gate"
        );
    }

    #[test]
    fn elevated_spread_outside_window_does_not_black_out() {
        // Control: the SAME wide 30-pip spread, but the fire bar is NOT at the
        // NY-close edge → window closed → no spread-BLACKOUT (the worker wouldn't
        // even sample). The spread is still wide enough to trip the SL-vs-spread
        // floor though: this long stop's SL is 50 pips, so `10 × 30 = 300` pips
        // >> 50 → the floor is violated, the widen pushes the stop to `10 × 30 =
        // 300` pips, and R collapses to 100/300 ≈ 0.33 < 1 → Declined via the
        // widen mirror (NOT a blackout). The point of this control is that the
        // outcome is *not* `SpreadBlackout`; the SL-widen decline is the correct
        // fall-through for a wide spread that's outside the blackout window.
        let intent = resolvable_long_stop();
        let shell = Shell::from_candle(&spread_fire_bar(NON_EDGE_TS, 1.1040, 0.0030).mid());
        let path = [spread_fire_bar(NON_EDGE_TS, 1.1040, 0.0030)];
        let out = simulate_fill(&intent, &shell, 0.0001, &path);
        assert!(
            !matches!(out, SimOutcome::SpreadBlackout { .. }),
            "a wide spread outside the close-edge window must not black out, got {out:?}"
        );
        assert_eq!(
            out,
            SimOutcome::Declined {
                name: "sl-widen-below-min-r".to_string(),
            },
            "the wide spread trips the SL-vs-spread floor; widening to 10x drops R<1 → declined"
        );
    }

    #[test]
    fn widened_sl_protects_the_leg_in_the_fill_path() {
        // The follow-up bug (BUG-sl-spread-floor…, 2026-07-01): the widen was
        // applied to the entry R-check but NOT to the stop the fill/exit sim
        // checks against, so a leg stopped out at the OLD un-widened SL even
        // though the live broker stop sat at the widened level.
        //
        // Geometry: long stop trigger 1.1050, SL 1.1000 (50 pips), TP 1.1150.
        // A 6-pip spread trips the 10× floor (60 > 50) → widen to 10× = 60 pips
        // → SL moves DOWN to 1.0990. R = 100/60 ≈ 1.67 ≥ 1 → entry stands.
        // An adverse bar then dips its bid to 1.0994: that crosses the ORIGINAL
        // 1.1000 SL (the buggy behaviour would stop out here) but NOT the
        // widened 1.0990 — so with the fix the leg survives and stays open.
        let intent = resolvable_long_stop();
        let fire = spread_fire_bar(NON_EDGE_TS, 1.1040, 0.0006); // 6-pip spread
        let shell = Shell::from_candle(&fire.mid());
        // Fill bar: ask reaches the 1.1050 trigger (long fills on the ask book).
        let fill_bar = ba_candle("2026-03-12T11:00:00Z", 1.1052, 1.1045, 1.1055, 1.1048);
        // Adverse bar: bid dips to 1.0994 — past the OLD SL, short of the widened.
        let dip_bar = ba_candle("2026-03-12T12:00:00Z", 1.1010, 1.0994, 1.1015, 1.0998);
        let path = [fire, fill_bar, dip_bar];
        let out = simulate_fill(&intent, &shell, 0.0001, &path);
        assert!(
            matches!(out, SimOutcome::FilledOpen { .. }),
            "leg must survive: the dip crosses the old SL but not the widened one, got {out:?}"
        );
    }

    #[test]
    fn mid_only_feed_never_blacks_out() {
        // A mid-only data source has bid == ask == mid → zero spread. Even a
        // fire bar at the close edge must never black out (we don't fabricate a
        // spread the data doesn't carry).
        let intent = resolvable_long_stop();
        let shell = Shell::from_candle(&candle(EDGE_TS, 1.1040, 1.1041, 1.1039, 1.1040).mid());
        let path = [candle(EDGE_TS, 1.1040, 1.1041, 1.1039, 1.1040)];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &path),
            SimOutcome::NeverFilled,
            "zero-spread mid-only bar must never black out"
        );
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

    /// The System-2 baseline (`original_stop`) is the stop the position actually
    /// carried — i.e. the signed SL **after** the System-1 entry spread floor —
    /// not the raw signed SL. This is the display-reconciliation fix: the widen
    /// must move from the *placed* stop, so the journal's order/widen/restore
    /// lines all key off one number (EUR/AUD `hs-eur-aud-3d0b5dda` showed three
    /// different SLs because System 2 used the un-floored signed level).
    #[test]
    fn widened_stop_at_baseline_is_the_floored_stop_not_the_signed_sl() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        // Long entry 1.1000, SIGNED SL 1.0995 (5p — inside the floor).
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0995, 1.1100);
        let shell = trigger_shell();
        // Fire bar carries a 3p spread (bid_c 1.10120, ask_c 1.10150) away from
        // the 1.1000 entry trigger so it doesn't fill on bar 0. 10× 3p = 30p →
        // the SL floors DOWN from 1.0995 to 1.0970.
        let fire = ba_candle("2026-06-17T10:30:00Z", 1.10150, 1.10120, 1.10150, 1.10120);
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // NY-close-edge wide bar → widen from the FLOORED baseline (1.0970).
        let wide = ba_candle("2026-06-17T21:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let path = [fire, fill_bar, wide];

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS)
            .expect("the NY-close-edge bar must trip the widen");
        assert!(
            (widen.original_stop - 1.0970).abs() < 1e-9,
            "baseline must be the floored stop 1.0970 (10× 3p), not the signed 1.0995; got {}",
            widen.original_stop
        );
        // And the widen moves further DOWN from the floored baseline.
        assert!(
            widen.widened_stop < 1.0970,
            "a long widen moves the SL further DOWN from the floored baseline"
        );
    }

    /// A long position whose post-fill path includes a wide-spread bar **on the
    /// NY-close edge** reports the widen: the bar's time, the original SL, and a
    /// stop moved DOWN (away from price for a long) by the clamped live spread.
    #[test]
    fn widened_stop_at_reports_the_widen_bar_for_a_long() {
        use trade_control_core::blackout_widen::{WIDEN_FLOOR_PIPS, widened_stop};
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        // Fire bar (skipped), then a zero-spread bar whose ASK reaches the 1.1000
        // long trigger → fill. Then a wide-spread bar (ask_c − bid_c = 1.10315 −
        // 1.10010 = 0.00305 = 30.5 pips, within the 22–40 clamp) that does NOT hit
        // SL/TP → widen. The wide bar sits at 21:00 UTC — the NY-close edge under
        // EDT (2026-06-17 is inside the DST window) — which the widen gate
        // requires, mirroring the live cron.
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        let wide = ba_candle("2026-06-17T21:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let spread_pips = (wide.ask_c - wide.bid_c) / 0.0001; // 30.5
        let path = [fire_bar(), fill_bar, wide];

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS)
            .expect("a 30.5-pip spread bar must trip the widen");
        assert_eq!(widen.at, wide.time);
        assert!((widen.original_stop - 1.0950).abs() < 1e-9);
        // 30.5 pips is within the 22–40 clamp, so widen by the live spread; long
        // moves DOWN.
        let expected = widened_stop(Direction::Long, 1.0950, spread_pips, 0.0001);
        assert!(
            (widen.widened_stop - expected).abs() < 1e-9,
            "expected {expected}, got {}",
            widen.widened_stop
        );
        assert!(
            widen.widened_stop < 1.0950,
            "a long widen moves the SL DOWN"
        );
    }

    /// A wide-spread bar that is **not** on the NY-close edge does NOT widen —
    /// the live System-2 cron only widens at the NY close (`is_ny_close_edge`),
    /// so the replay mirror must too. Without the gate this bar (12:00 UTC, a
    /// 30.5-pip spread) would trip the widen; with it, it's ignored. This is the
    /// parity gap from the EUR/AUD `hs-eur-aud-3d0b5dda` journal bug: the report
    /// mistook a NY-close-edge widen for a "wrong bar", but the real defect was
    /// the replay widening on *any* wide bar, not only the NY-close one.
    #[test]
    fn widened_stop_at_ignores_a_wide_bar_off_the_ny_close_edge() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // 30.5-pip spread — well over the floor — but 12:00 UTC is not the NY
        // close (21:00 UTC under EDT). Gate rejects it.
        let wide = ba_candle("2026-06-17T12:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let path = [fire_bar(), fill_bar, wide];
        assert_eq!(
            widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS),
            None,
            "a wide bar off the NY-close edge must not widen"
        );
    }

    /// Two wide-spread bars — one off the edge (12:00 UTC), one on it (21:00
    /// UTC). The widen must land on the NY-close bar, not the earlier off-edge
    /// one, proving the loop skips non-edge bars rather than firing on the first
    /// wide bar it sees.
    #[test]
    fn widened_stop_at_widens_on_the_ny_close_bar_not_the_first_wide_bar() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // Off-edge wide bar (12:00 UTC) — must be skipped.
        let off_edge = ba_candle("2026-06-17T12:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        // NY-close-edge wide bar (21:00 UTC under EDT) — must be the widen bar.
        let on_edge = ba_candle("2026-06-17T21:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let path = [fire_bar(), fill_bar, off_edge, on_edge];
        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS)
            .expect("the NY-close-edge bar must trip the widen");
        assert_eq!(
            widen.at, on_edge.time,
            "widen must land on the NY-close bar, not the first wide bar"
        );
    }

    /// A **baked** spread-hour instrument (TN-named `EUR/USD`, whose baked mask
    /// has bit 21 set with a ~5p p90) widens by the baked p90 via
    /// `spread_hour_widen_size`, NOT the legacy 22p `clamp_widen` floor — this
    /// is the whole point of the per-instrument path. The bar's live spread is
    /// tight (2p), so the legacy path would have needed the trigger *and* would
    /// have floored to 22p; the baked path fires on the mask alone and widens by
    /// ~5p. Depends on the committed sampler baseline (EUR/USD 21:00 spike).
    #[test]
    fn widened_stop_at_uses_baked_p90_for_a_sampled_instrument() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        use trade_control_core::spread_blackout::spread_hour_widen_pips;

        let mut intent = long_stop_intent();
        intent.instrument = "EUR/USD".into(); // TN name → in the baked table
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();

        // Guard: the baseline must actually carry EUR/USD's 21:00 spread hour,
        // else this test is vacuous. 21:00 UTC on an EDT date is a spread hour.
        let at21 = ts("2026-06-17T21:00:00Z");
        let baked = spread_hour_widen_pips("EUR/USD", at21)
            .expect("EUR/USD must have a baked 21:00 spread hour");

        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // 21:00 UTC bar with a TIGHT 2p spread — the baked mask fires anyway.
        let tight = ba_candle("2026-06-17T21:00:00Z", 1.10010, 1.10005, 1.10030, 1.10025);
        let path = [fire_bar(), fill_bar, tight];

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS)
            .expect("the baked spread hour must trip the widen even on a tight-spread bar");
        assert_eq!(widen.at, tight.time);
        // Widen distance = baked p90 (~5p), NOT the 22p legacy floor. Long ⇒ SL
        // moves DOWN from 1.0950 by baked p90 pips.
        let expected = 1.0950 - baked * 0.0001;
        assert!(
            (widen.widened_stop - expected).abs() < 1e-9,
            "widened by baked p90 {baked}p (expected SL {expected}), got {}",
            widen.widened_stop,
        );
        // Sanity: the baked p90 is much smaller than the legacy 22p floor, so
        // this genuinely exercises the new path.
        assert!(
            baked < WIDEN_FLOOR_PIPS,
            "baked p90 {baked} should be < 22p floor"
        );
    }

    /// The widen is **transient**: once a post-widen bar's spread recovers to/
    /// under the 4-pip cutoff, `restored_at` reports that bar — mirroring the
    /// live recovery watcher (`blackout_watch`). This is what lets the replay
    /// journal show the stop snapping back instead of a permanent widen (the
    /// EUR/AUD `hs-eur-aud-3d0b5dda` "permanent widen" journal question).
    #[test]
    fn widened_stop_at_reports_restore_on_spread_recovery() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // Widen bar: 30.5p spread on the NY-close edge (21:00 UTC, EDT).
        let wide = ba_candle("2026-06-17T21:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        // Next bar: spread back to 2p (≤ the 4p recovered cutoff) → restore here.
        let recovered = ba_candle("2026-06-17T22:00:00Z", 1.10010, 1.10005, 1.10030, 1.10025);
        let path = [fire_bar(), fill_bar, wide, recovered];

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS)
            .expect("the NY-close-edge bar must trip the widen");
        assert_eq!(widen.at, wide.time);
        assert_eq!(
            widen.restored_at,
            Some(recovered.time),
            "the widen must restore at the first recovered-spread bar"
        );
    }

    /// If the spread never recovers before the path ends, `restored_at` is
    /// `None` (the widen would still be active at the window's end — the 3-hour
    /// backstop hasn't landed within these bars either).
    #[test]
    fn widened_stop_at_restore_is_none_when_spread_stays_wide() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        let wide = ba_candle("2026-06-17T21:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        // Still ~30p an hour later — under the 3h backstop, spread not recovered.
        let still_wide = ba_candle("2026-06-17T22:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let path = [fire_bar(), fill_bar, wide, still_wide];

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS)
            .expect("the NY-close-edge bar must trip the widen");
        assert_eq!(
            widen.restored_at, None,
            "no recovery, no backstop → no restore"
        );
    }

    /// The 3-hour backstop restores even if the spread stays elevated — the live
    /// watcher's Safety Rule 2. A bar ≥ 3h after the widen restores regardless
    /// of its (still-wide) spread.
    #[test]
    fn widened_stop_at_restore_fires_on_the_backstop() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        let wide = ba_candle("2026-06-17T21:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        // Still wide at +1h/+2h (no recovery), then a bar at +3h → backstop.
        let hour1 = ba_candle("2026-06-17T22:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let hour2 = ba_candle("2026-06-17T23:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let backstop = ba_candle("2026-06-18T00:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let path = [fire_bar(), fill_bar, wide, hour1, hour2, backstop];

        let widen = widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS)
            .expect("the NY-close-edge bar must trip the widen");
        assert_eq!(
            widen.restored_at,
            Some(backstop.time),
            "the 3h backstop must restore even with the spread still wide"
        );
    }

    /// No bar's spread reaches the trigger → no widen.
    #[test]
    fn widened_stop_at_is_none_when_spread_stays_tight() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // 2-pip spread bar — well under the 22-pip floor.
        let tight = ba_candle("2026-06-17T12:00:00Z", 1.10010, 1.10005, 1.10030, 1.10025);
        let path = [fire_bar(), fill_bar, tight];
        assert_eq!(
            widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS),
            None
        );
    }

    /// A position stopped out before any wide-spread bar reports no widen — the
    /// original stop was still the live one when the position closed.
    #[test]
    fn widened_stop_at_is_none_when_stopped_before_widen() {
        use trade_control_core::blackout_widen::WIDEN_FLOOR_PIPS;
        let mut intent = long_stop_intent();
        i_set_levels(&mut intent, 1.1000, 1.0950, 1.1100);
        let shell = trigger_shell();
        let fill_bar = candle("2026-06-17T11:00:00Z", 1.1000, 1.1001, 1.0999, 1.1000);
        // Hits the 1.0950 SL on the bid book (long exits on bid) before any
        // wide-spread bar.
        let to_sl = candle("2026-06-17T12:00:00Z", 1.0990, 1.0991, 1.0949, 1.0951);
        let wide = ba_candle("2026-06-17T13:00:00Z", 1.10015, 1.10010, 1.10315, 1.10310);
        let path = [fire_bar(), fill_bar, to_sl, wide];
        assert_eq!(
            widened_stop_at(&intent, &shell, 0.0001, &path, WIDEN_FLOOR_PIPS),
            None
        );
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
            sweep_reason(&intent, &shell, 0.0001, &path, &[]),
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
            sweep_reason(&intent, &shell, 0.0001, &path, &[]),
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
            sweep_reason(&intent, &shell, 0.0001, &path, &[]),
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
        assert_eq!(sweep_reason(&intent, &shell, 0.0001, &path, &[]), None);
    }

    /// A Market entry never rests, so a (degenerate) Market `NeverFilled` is not
    /// a swept order → `None`.
    #[test]
    fn sweep_reason_is_none_for_market_entry() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Market);
        let shell = trigger_shell();
        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &[fire_bar()], &[]),
            None
        );
    }

    /// A never-triggered stop-entry whose resting bars fall inside a market-hours
    /// blackout window → swept as `Blackout`. Blackout takes priority over a
    /// same-bar SL-breach (worker `sweep_one` branch order: blackout before the
    /// stale-price SL check). An empty window slice never fires this branch.
    #[test]
    fn sweep_reason_reports_market_blackout() {
        use trade_control_core::intent::NoEntryWindow;

        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // trigger 1.1050; SL 1.1000
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.expiry_bars = None;
        let shell = trigger_shell();

        // The 12:00Z bar both closes past the SL AND sits inside the blackout
        // window (UTC minute 720 = 12:00). Blackout must win over SL-breach.
        let path = [
            fire_bar(),
            candle("2026-06-17T11:00:00Z", 1.1041, 1.1045, 1.1038, 1.1043), // in-window, no breach
            candle("2026-06-17T12:00:00Z", 1.1010, 1.1012, 1.0990, 1.0995), // blackout + past SL
        ];
        // 11:55–12:05 UTC blackout (minutes 715..=725) catches the 12:00 bar.
        let windows = [NoEntryWindow::new(715, 725)];
        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &path, &windows),
            Some((SweepReason::Blackout, ts("2026-06-17T12:00:00Z")))
        );
        // Same path with no windows → falls through to SL-breach (not blackout).
        assert_eq!(
            sweep_reason(&intent, &shell, 0.0001, &path, &[]),
            Some((SweepReason::SlBreached, ts("2026-06-17T12:00:00Z")))
        );
    }
}
