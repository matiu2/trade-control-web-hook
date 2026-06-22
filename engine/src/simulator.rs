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
    let entry_book = book_for(Leg::Entry, dir);
    let (fill_at, entry_price, rest) = match resolved.entry {
        ResolvedEntry::Market { reference_price } => (shell.time, reference_price, candles),
        ResolvedEntry::Stop { trigger_price } | ResolvedEntry::Limit { trigger_price } => {
            match candles
                .iter()
                .position(|c| book_crosses(c, entry_book, trigger_price))
            {
                Some(i) => (candles[i].time, trigger_price, &candles[i + 1..]),
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
            on_too_close: None,
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
            on_too_close: None,
        });
        let shell = trigger_shell();

        // Path A: a candle reaches 1.1050 (fills), a later one reaches 1.1150 (TP).
        let tp_path = [
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
        let no_fill = [candle(
            "2026-06-17T11:00:00Z",
            1.1041,
            1.1045,
            1.1038,
            1.1043,
        )];
        assert_eq!(
            simulate_fill(&intent, &shell, 0.0001, &no_fill),
            SimOutcome::NeverFilled
        );

        // Path D: fills but neither level touched → FilledOpen.
        let still_open = [
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
            on_too_close: None,
        });
        let shell = trigger_shell();
        // Fills @1.1050, then a candle reaches SL 1.1000 → StoppedOut.
        let sl_path = [
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
            on_too_close: None,
        });
        let shell = trigger_shell();
        // One candle fills AND spans both SL and TP → pessimistic: StoppedOut.
        let both = [
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
            on_too_close: None,
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
        let path = [fill_bar, sl_bar];

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
        let near_miss = ba_candle("2026-06-17T10:30:00Z", 1.1027, 1.1008, 1.1029, 1.1012);
        assert!(
            matches!(
                simulate_fill(&intent, &shell, 0.0001, &[fill_bar, near_miss]),
                SimOutcome::FilledOpen { .. }
            ),
            "ask high 1.1029 misses the 1.1030 SL → still open"
        );
    }
}
