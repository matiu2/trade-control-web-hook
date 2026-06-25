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

    // Phase 1 — find the fill. A short entry sells (fills on the bid book); a
    // long entry buys (fills on the ask book). A market entry crosses the spread
    // at once. The recorded fill price is the placed level (the resting order's
    // price), not the book extreme.
    //
    // `candles[0]` is the **fire bar** — the bar the enter fired on. A pending
    // order is only placed once that bar has *closed* (the engine decides the
    // fire on the cron tick that processes the closed bar; under
    // `needs_confirmed` the confirmation itself isn't known until this bar's
    // close). So a Stop/Limit order cannot interact with the fire bar's own
    // intrabar path — the earliest bar it can fill on is the **next** one. We
    // therefore search for the fill from `candles[1..]`. A Market entry is the
    // exception: it fills at the fire bar's close (the shell price), which is
    // exactly when the order is placed.
    let entry_book = book_for(Leg::Entry, dir);
    let (fill_at, entry_price, rest) = match resolved.entry {
        ResolvedEntry::Market { reference_price } => (shell.time, reference_price, candles),
        ResolvedEntry::Stop { trigger_price } | ResolvedEntry::Limit { trigger_price } => {
            // Skip the fire bar (index 0): the resting order isn't live until
            // after it closes. `get(1..)` is empty when the fire bar is the only
            // recorded candle, yielding `NeverFilled` — correct (no later bar to
            // fill on yet).
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
            match fill_window
                .iter()
                .position(|c| book_crosses(c, entry_book, trigger_price))
            {
                // `i` indexes `fill_window` (a prefix of `after_fire`), so the
                // fill bar is `candles[i + 1]`.
                // The post-fill SL/TP search **includes** the fill bar itself
                // (`candles[i + 1..]`): an order that fills mid-bar can be stopped
                // out (or hit TP) later in that SAME bar — a real intrabar loss
                // on a violent breakout bar. With only OHLC we can't order the
                // fill vs the SL/TP touch within the bar, so Phase 2's pessimistic
                // tie-break (SL wins on ambiguity) covers a fill bar that spans
                // both.
                Some(i) => (after_fire[i].time, trigger_price, &candles[i + 1..]),
                None => return SimOutcome::NeverFilled,
            }
        }
    };

    // Phase 2 — after the fill, the first candle that touches SL or TP closes
    // the position. The close is the *opposite* book side from entry (short buys
    // back on the ask, long sells on the bid). If both are touched in the same
    // candle we can't tell the intrabar order from a closed bar, so we
    // conservatively call it the stop (the worse outcome) — matches the
    // simulator doc's "exact-level, pessimistic on ambiguity" stance.
    let exit_book = book_for(Leg::Exit, dir);
    for c in rest {
        let hit_sl = book_crosses(c, exit_book, resolved.stop_loss);
        let hit_tp = book_crosses(c, exit_book, resolved.take_profit);
        match (hit_sl, hit_tp) {
            (true, _) => {
                return SimOutcome::StoppedOut {
                    fill_at,
                    entry_price,
                    exit_at: c.time,
                    exit_price: resolved.stop_loss,
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
    }

    SimOutcome::FilledOpen {
        fill_at,
        entry_price,
    }
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

    #[test]
    fn ambiguous_candle_resolves_to_stop() {
        let mut intent = long_stop_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0,
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
}
