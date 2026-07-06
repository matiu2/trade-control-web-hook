//! A fake [`Broker`] for offline multi-shot replay.
//!
//! The shared multi-shot gate (`trade_control_core::retry_gate::evaluate`) is
//! async and asks the **broker** whether a prior attempt is still open before
//! allowing a re-entry. Live, that broker is TradeNation/OANDA. Offline, this
//! `ReplayBroker` approximates the answer from candles: each placed attempt is
//! re-simulated with [`simulate_fill`] **up to the bar the gate is asking on**
//! (time-accurate — a re-entry only clears once the prior attempt has really
//! closed by that bar), and the [`SimOutcome`] is mapped to an [`AttemptState`].
//!
//! Only the retry-gate-relevant methods do real work
//! (`lookup_attempt_state`, `list_open_positions`, `cancel_order`); the replay
//! never places real orders, so `place_entry` and the rest are stubs.

use std::cell::RefCell;

use chrono::{DateTime, Utc};
use trade_control_core::broker::{
    AmendError, AttemptState, BidAskCandle, Broker, CancelError, Candle, CandleError, EntryError,
    EntryRequest, Granularity, LookupError, OpenPosition, PendingOrder, Quote,
};
use trade_control_core::intent::{Intent, Shell};
use trade_control_engine::{SimOutcome, simulate_fill};

/// One placed attempt the gate may later ask about, with the geometry needed to
/// re-simulate it. `order_id` is what [`Broker::place_entry`] handed back (the
/// retry gate keys on it); `shell` + `intent` resolve the entry/SL/TP.
#[derive(Clone)]
struct PlacedAttempt {
    order_id: String,
    intent: Intent,
    shell: Shell,
    /// Set once the gate cancels this resting order (supersede path). A
    /// cancelled attempt resolves to [`AttemptState::Cancelled`] regardless of
    /// the price path.
    cancelled: bool,
}

/// The geometry the replay loop arms before each `run_enter` so this broker's
/// `place_entry` can mint a correlatable order id and record the attempt. The
/// real dispatch (`run_enter`) calls `broker.place_entry` with only an
/// `EntryRequest`, which lacks the intent + shell the offline prior-attempt
/// resolver needs — so the loop hands them in out-of-band here.
#[derive(Clone)]
struct ArmedPlacement {
    order_id: String,
    intent: Intent,
    shell: Shell,
}

/// Offline broker that resolves prior-attempt state from the candle window.
pub struct ReplayBroker {
    /// The full pulled bid/ask candle window (warm-up + live), ascending. Each
    /// lookup re-simulates an attempt against the prefix up to the asking bar,
    /// filling each leg on the real book side.
    candles: Vec<BidAskCandle>,
    pip_size: f64,
    /// The bar the gate is currently asking about — its open time. Set by the
    /// replay loop before each `evaluate`, so `lookup_attempt_state` bounds its
    /// simulation at this bar (time-accurate prior-state resolution).
    as_of: RefCell<DateTime<Utc>>,
    placed: RefCell<Vec<PlacedAttempt>>,
    /// The placement the loop armed for the next `run_enter` (its intent, shell,
    /// and the order id `place_entry` should return). Consumed by `place_entry`.
    armed: RefCell<Option<ArmedPlacement>>,
}

impl ReplayBroker {
    pub fn new(candles: Vec<BidAskCandle>, pip_size: f64) -> Self {
        let last = candles.last().map(|c| c.time).unwrap_or_else(Utc::now);
        Self {
            candles,
            pip_size,
            as_of: RefCell::new(last),
            placed: RefCell::new(Vec::new()),
            armed: RefCell::new(None),
        }
    }

    /// Point all subsequent prior-attempt lookups at `as_of` (the open time of
    /// the bar the gate is evaluating). Call before each `retry_gate::evaluate`.
    pub fn set_as_of(&self, as_of: DateTime<Utc>) {
        *self.as_of.borrow_mut() = as_of;
    }

    /// Arm the placement for the next `run_enter`: the order id `place_entry`
    /// should return and the intent + shell needed to resolve this attempt's
    /// later state. Call right before dispatching the enter; `place_entry`
    /// consumes it. `order_id` must match what the gate stores on the
    /// `EntryAttempt` (`run_enter` stamps `place_entry`'s return there), so the
    /// minted id is the standard `{intent.id}-{attempt_no}` form.
    pub fn arm_placement(&self, order_id: String, intent: Intent, shell: Shell) {
        *self.armed.borrow_mut() = Some(ArmedPlacement {
            order_id,
            intent,
            shell,
        });
    }

    /// Register a placed attempt so a later lookup can resolve it. `order_id`
    /// must match what the gate stored on the `EntryAttempt` (the replay uses
    /// the same id when it `record_placement`s).
    fn record_attempt(&self, order_id: String, intent: Intent, shell: Shell) {
        self.placed.borrow_mut().push(PlacedAttempt {
            order_id,
            intent,
            shell,
            cancelled: false,
        });
    }

    /// The order ids the gate has cancelled so far (the cancel-and-replace
    /// path — a later sibling/re-entry superseded a still-resting order). The
    /// replay loop reads this after each gate call to stamp the superseded
    /// `Fire` so the report shows it as cancelled, not a fabricated fill.
    pub fn cancelled_order_ids(&self) -> Vec<String> {
        self.placed
            .borrow()
            .iter()
            .filter(|a| a.cancelled)
            .map(|a| a.order_id.clone())
            .collect()
    }

    /// Candles up to and including the `as_of` bar — the slice a prior attempt
    /// is simulated against. Bounding here is what makes re-entry time-accurate.
    fn window_to_as_of(&self) -> Vec<BidAskCandle> {
        let as_of = *self.as_of.borrow();
        self.candles
            .iter()
            .filter(|c| c.time <= as_of)
            .cloned()
            .collect()
    }

    /// The bid/ask candle at the current `as_of` bar (the bar `run_enter` is
    /// firing on, since the replay loop calls `set_as_of(fire_bar.time)` right
    /// before dispatching). This is the closed fire bar whose book the live
    /// worker would sample with a `get_quote` round-trip. Falls back to the last
    /// candle at/before `as_of` if the exact open time isn't present (it always
    /// is in the replay's closed loop, but stay robust).
    fn candle_at_as_of(&self) -> Option<&BidAskCandle> {
        let as_of = *self.as_of.borrow();
        self.candles.iter().rfind(|c| c.time <= as_of)
    }

    /// Resolve a placed attempt's current state from its price path up to
    /// `as_of`. The attempt's own candles are those at/after its shell time
    /// (the bar it fired on) within the bounded window.
    fn resolve(&self, attempt: &PlacedAttempt) -> AttemptState {
        if attempt.cancelled {
            return AttemptState::Cancelled;
        }
        let window = self.window_to_as_of();
        // Forward path = candles from the firing bar onward (simulate_fill walks
        // these to find the fill, then the SL/TP touch).
        let forward: Vec<BidAskCandle> = window
            .into_iter()
            .filter(|c| c.time >= attempt.shell.time)
            .collect();
        match simulate_fill(&attempt.intent, &attempt.shell, self.pip_size, &forward) {
            SimOutcome::StoppedOut { .. } => {
                AttemptState::ClosedLossOrBreakeven { realized_pl: -1.0 }
            }
            SimOutcome::TookProfit { .. } => AttemptState::ClosedWin { realized_pl: 1.0 },
            SimOutcome::FilledOpen { .. } => AttemptState::OpenPosition {
                broker_trade_id: format!("{}-pos", attempt.order_id),
            },
            // Not filled by the asking bar = a still-**resting** order, exactly
            // what the real broker reports as `Pending`. This is load-bearing
            // for strategy-v2: a sibling enter (QM limit vs break-and-close stop)
            // firing on a later bar must see the prior resting order as `Pending`
            // so the gate **cancels and replaces** it (cancel-and-replace), and
            // so a still-resting order can't go on to fill alongside the new one.
            // Returning `Cancelled` here (the old behaviour) silently let both
            // orders rest+fill → overlapping positions (Bug 1 + Bug 2). A
            // genuinely cancelled order is caught above by `attempt.cancelled`.
            // (`expiry_bars`-driven expiry is folded into `simulate_fill`'s fill
            // window, so an expired order resolves to `NeverFilled`/`Pending`
            // here too — these v2 plans don't set `expiry_bars`, and the gate's
            // cap/window bound the re-entry count regardless.)
            SimOutcome::NeverFilled => AttemptState::Pending,
            // Declined / spread-blackout / unresolved — no order ever went on;
            // the slot is free.
            SimOutcome::Declined { .. }
            | SimOutcome::SpreadBlackout { .. }
            | SimOutcome::Unresolved(_) => AttemptState::Cancelled,
        }
    }
}

impl Broker for ReplayBroker {
    async fn place_entry(
        &self,
        _max_risk_pct: f64,
        _max_open_positions: u32,
        _req: &EntryRequest<'_>,
    ) -> Result<String, EntryError> {
        // The real dispatch (`run_enter`) calls this to "place" the order. The
        // replay loop armed the geometry out-of-band (intent + shell + the order
        // id to return) because `EntryRequest` lacks what the offline
        // prior-attempt resolver needs. Record the attempt so a later
        // `lookup_attempt_state` can resolve it, and hand back the armed id —
        // which `run_enter` then stamps onto the `EntryAttempt` row, keeping the
        // gate's correlation intact.
        let armed = self.armed.borrow_mut().take();
        match armed {
            Some(a) => {
                self.record_attempt(a.order_id.clone(), a.intent, a.shell);
                Ok(a.order_id)
            }
            // No armed placement means the loop dispatched an enter without
            // arming first — a wiring bug, not a broker condition. Fail the
            // placement loudly rather than fabricate an id.
            None => {
                tracing::error!(
                    "ReplayBroker::place_entry called with no armed placement — replay wiring bug"
                );
                Err(EntryError::OrderRejected)
            }
        }
    }

    async fn close_positions(&self, _instrument: &str) -> bool {
        false
    }

    async fn cancel_pending_for_instrument(&self, _instrument: &str) -> usize {
        0
    }

    async fn lookup_attempt_state(
        &self,
        _instrument: &str,
        broker_order_id: &str,
        _broker_trade_id: Option<&str>,
    ) -> Result<AttemptState, LookupError> {
        let placed = self.placed.borrow();
        match placed.iter().find(|a| a.order_id == broker_order_id) {
            Some(a) => Ok(self.resolve(a)),
            // The gate only asks about ids we placed; an unknown id means the
            // attempt was never recorded — treat as Unknown (fail-safe in the
            // gate, though this shouldn't happen in the replay's closed loop).
            None => Ok(AttemptState::Unknown),
        }
    }

    async fn cancel_order(
        &self,
        _account_id: &str,
        broker_order_id: &str,
    ) -> Result<(), CancelError> {
        if let Some(a) = self
            .placed
            .borrow_mut()
            .iter_mut()
            .find(|a| a.order_id == broker_order_id)
        {
            a.cancelled = true;
        }
        Ok(())
    }

    async fn get_quote(&self, _instrument: &str) -> Result<Quote, LookupError> {
        // The shared entry gates (spread-blackout + SL-vs-spread floor in
        // `dispatch::run_enter`) sample the live spread via this round-trip. The
        // replay candles carry the real book (`bid_c`/`ask_c`), so synthesize the
        // quote from the fire bar's close rather than failing open: that lets the
        // offline replay REPRODUCE a spread rejection the live worker would make,
        // tightening replay↔live parity.
        //
        // Fidelity caveat: a closed bar's `bid_c`/`ask_c` is the spread *at the
        // bar's close*, a coarse proxy for the live worker's instant-of-fire
        // sample. It captures sustained-wide spreads — exactly the post-NY-close
        // liquidity trough the spread-blackout window targets — but not a brief
        // intrabar spike that retraces by the close. So the replay reproduces the
        // common case (sustained wide) and under-reports the sub-bar-spike edge.
        // Better than the old unconditional fail-open, which reproduced nothing.
        match self.candle_at_as_of() {
            Some(c) => Ok(Quote {
                bid: c.bid_c,
                ask: c.ask_c,
            }),
            // No candle at/before `as_of` — should never happen in the replay's
            // closed loop (the fire bar is always present), but if it does, fail
            // open the same way the live worker does on a transient quote error.
            None => Err(LookupError::Transient),
        }
    }

    async fn list_open_positions(
        &self,
        _account_id: &str,
    ) -> Result<Vec<OpenPosition>, LookupError> {
        // The Bug #11 backstop: report a synthetic open position for any placed
        // attempt that resolves to OpenPosition by the asking bar, keyed back to
        // its order id so the gate's correlation matches.
        let placed = self.placed.borrow();
        let positions = placed
            .iter()
            .filter_map(|a| match self.resolve(a) {
                AttemptState::OpenPosition { broker_trade_id } => Some(OpenPosition {
                    instrument: a.intent.instrument.clone(),
                    direction: a
                        .intent
                        .direction
                        .unwrap_or(trade_control_core::intent::Direction::Long),
                    stop_loss: None,
                    take_profit: None,
                    position_id: broker_trade_id,
                    order_id: a.order_id.clone(),
                    stake: 1.0,
                }),
                _ => None,
            })
            .collect();
        Ok(positions)
    }

    async fn amend_stop(
        &self,
        _account_id: &str,
        _position_or_order_id: &str,
        _new_stop: f64,
    ) -> Result<(), AmendError> {
        Ok(())
    }

    async fn list_pending_orders(
        &self,
        _account_id: &str,
    ) -> Result<Vec<PendingOrder>, LookupError> {
        Ok(Vec::new())
    }

    async fn get_candles(
        &self,
        _instrument: &str,
        _granularity: Granularity,
        _since: DateTime<Utc>,
        _now: DateTime<Utc>,
    ) -> Result<Vec<Candle>, CandleError> {
        // The replay feeds MID candles directly; the gate never fetches them.
        Ok(Vec::new())
    }

    async fn get_bidask_candles(
        &self,
        _instrument: &str,
        _granularity: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<BidAskCandle>, CandleError> {
        // THE shared bar feeder for the entry SL-spread floor: `run_enter`'s
        // `windowed_entry_spread` calls this to average the last N bars' spread
        // — the SAME code path the live worker drives through its real broker.
        // The replay serves it from its own recorded series, so worker and
        // replay size the floor off an identical statistic (no hand-sliced
        // window, no duplicated floor logic → no drift).
        //
        // Bound the window to `(since, now]`, clamped at the `as_of` bar so a
        // fire never sees candles after the bar it fired on (time-accurate,
        // same discipline as `window_to_as_of`). Closed bars only — the replay
        // series is already all-closed.
        if since >= now {
            return Err(CandleError::BadRange);
        }
        let as_of = *self.as_of.borrow();
        let upper = now.min(as_of);
        Ok(self
            .candles
            .iter()
            .filter(|c| c.time > since && c.time <= upper)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A bid==ask==mid bar (zero spread) — the books equal the mid OHLC, so the
    /// fill tests read as plain prices while still exercising the bid/ask path.
    fn candle(epoch: i64, c: f64) -> BidAskCandle {
        let (o, h, l) = (c, c + 0.001, c - 0.001);
        BidAskCandle {
            time: Utc.timestamp_opt(epoch, 0).unwrap(),
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

    /// A minimal short stop-entry enter intent (serde-built, the pattern the
    /// other replay tests use) anchored to absolute levels so resolution needs
    /// no signal geometry: entry stop at 1.1000, SL 1.1020, TP 1.0950.
    fn short_enter_intent() -> Intent {
        serde_json::from_str(
            r#"{
                "v": 1,
                "id": "t-enter",
                "not_after": "2026-06-20T00:00:00Z",
                "action": "enter",
                "instrument": "EUR/USD",
                "direction": "short",
                "entry": { "type": "stop", "from": "close", "offset_pips": 0.0, "at": 1.1000 },
                "stop_loss": { "absolute": 1.1020 },
                "take_profit": { "absolute": 1.0950 },
                "broker": "tradenation",
                "trade_id": "t",
                "max_retries": 5
            }"#,
        )
        .expect("valid enter intent")
    }

    /// A bar carrying an explicit bid/ask close spread, so `get_quote` has a
    /// non-zero book to surface. Mid OHLC are left at `c` for simplicity (the
    /// quote path reads only the bid/ask closes).
    fn spread_candle(epoch: i64, bid_c: f64, ask_c: f64) -> BidAskCandle {
        let mid = (bid_c + ask_c) / 2.0;
        BidAskCandle {
            time: Utc.timestamp_opt(epoch, 0).unwrap(),
            o: mid,
            h: mid + 0.001,
            l: mid - 0.001,
            c: mid,
            bid_o: bid_c,
            bid_h: bid_c + 0.001,
            bid_l: bid_c - 0.001,
            bid_c,
            ask_o: ask_c,
            ask_h: ask_c + 0.001,
            ask_l: ask_c - 0.001,
            ask_c,
        }
    }

    #[tokio::test]
    async fn get_quote_synthesizes_the_as_of_bar_book() {
        // Two bars with different spreads; `get_quote` must reflect whichever
        // bar `as_of` points at (the fire bar the worker would sample).
        let tight = spread_candle(0, 1.10000, 1.10002); // 0.2 pip
        let wide = spread_candle(3600, 1.10000, 1.10050); // 5.0 pip (blackout-class)
        let b = ReplayBroker::new(vec![tight, wide], 0.0001);

        // As-of the tight bar → tight quote.
        b.set_as_of(Utc.timestamp_opt(0, 0).unwrap());
        let q0 = b.get_quote("EUR/USD").await.unwrap();
        assert_eq!(q0.bid, 1.10000);
        assert_eq!(q0.ask, 1.10002);
        assert!((q0.spread() / 0.0001 - 0.2).abs() < 1e-9, "0.2 pip spread");

        // As-of the wide bar → wide quote (the spread the blackout gate rejects).
        b.set_as_of(Utc.timestamp_opt(3600, 0).unwrap());
        let q1 = b.get_quote("EUR/USD").await.unwrap();
        assert_eq!(q1.bid, 1.10000);
        assert_eq!(q1.ask, 1.10050);
        assert!((q1.spread() / 0.0001 - 5.0).abs() < 1e-9, "5.0 pip spread");
    }

    #[tokio::test]
    async fn get_quote_fails_open_with_no_candle_before_as_of() {
        // `as_of` before any candle → no book to sample → transient (fail open),
        // matching the live worker's behaviour on a quote-endpoint hiccup.
        let b = ReplayBroker::new(vec![spread_candle(3600, 1.10000, 1.10002)], 0.0001);
        b.set_as_of(Utc.timestamp_opt(0, 0).unwrap());
        let err = b.get_quote("EUR/USD").await.unwrap_err();
        assert_eq!(err, LookupError::Transient);
    }

    #[tokio::test]
    async fn unknown_order_id_resolves_unknown() {
        let b = ReplayBroker::new(vec![candle(0, 1.10)], 0.0001);
        let st = b
            .lookup_attempt_state("EUR/USD", "nope", None)
            .await
            .unwrap();
        assert_eq!(st, AttemptState::Unknown);
    }

    #[tokio::test]
    async fn cancelled_order_resolves_cancelled() {
        // Candles that would fill + stop the short (so absent the cancel it'd be
        // ClosedLossOrBreakeven); the cancel must override to Cancelled.
        let candles = vec![candle(0, 1.1000), candle(3600, 1.1025)];
        let b = ReplayBroker::new(candles, 0.0001);
        let shell = Shell::from_candle(&candle(0, 1.1000).mid());
        b.record_attempt("o1".into(), short_enter_intent(), shell);
        b.cancel_order("", "o1").await.unwrap();
        let st = b.lookup_attempt_state("EUR/USD", "o1", None).await.unwrap();
        assert_eq!(st, AttemptState::Cancelled);
    }

    #[tokio::test]
    async fn open_then_closed_as_the_asof_bar_advances() {
        // The attempt fires on bar 0 (its shell bar); a resting order isn't live
        // until that bar closes, so the fill can only land on bar 1 onward (the
        // fire-bar skip in `simulate_fill`). Here the bid reaches the 1.1000
        // sell-stop on bar 1 (fill), then the SL at 1.1020 is hit on bar 2. So
        // as-of bar 0 → not filled yet, but the order is **resting** (Pending);
        // as-of bar 1 → OpenPosition; as-of bar 2 → ClosedLossOrBreakeven.
        let fire_bar = candle(0, 1.1010); // shell/fire bar — above the trigger, no fill
        let fill_bar = candle(3600, 1.1000); // bid reaches the 1.1000 sell-stop
        let sl_bar = candle(7200, 1.1021); // SL 1.1020 hit
        let candles = vec![fire_bar, fill_bar, sl_bar];
        let b = ReplayBroker::new(candles, 0.0001);
        let shell = Shell::from_candle(&fire_bar.mid());
        b.record_attempt("o1".into(), short_enter_intent(), shell);

        // As-of the fire bar: order placed but not yet filled (can't fill on its
        // own fire bar). It's a live **resting** order → Pending, exactly what the
        // real broker reports — so a sibling enter would cancel-and-replace it.
        b.set_as_of(Utc.timestamp_opt(0, 0).unwrap());
        let at_fire = b.lookup_attempt_state("EUR/USD", "o1", None).await.unwrap();
        assert!(
            matches!(at_fire, AttemptState::Pending),
            "fire bar can't fill the resting order, but it's resting → Pending, got {at_fire:?}"
        );

        // As-of bar 1: filled, not yet stopped → open.
        b.set_as_of(Utc.timestamp_opt(3600, 0).unwrap());
        let early = b.lookup_attempt_state("EUR/USD", "o1", None).await.unwrap();
        assert!(
            matches!(early, AttemptState::OpenPosition { .. }),
            "filled on bar 1, not yet stopped → open, got {early:?}"
        );

        // As-of bar 2: SL hit → closed.
        b.set_as_of(Utc.timestamp_opt(7200, 0).unwrap());
        let late = b.lookup_attempt_state("EUR/USD", "o1", None).await.unwrap();
        assert!(
            matches!(late, AttemptState::ClosedLossOrBreakeven { .. }),
            "SL hit by bar 2 → closed, got {late:?}"
        );
    }
}
