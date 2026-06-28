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
}

impl ReplayBroker {
    pub fn new(candles: Vec<BidAskCandle>, pip_size: f64) -> Self {
        let last = candles.last().map(|c| c.time).unwrap_or_else(Utc::now);
        Self {
            candles,
            pip_size,
            as_of: RefCell::new(last),
            placed: RefCell::new(Vec::new()),
        }
    }

    /// Point all subsequent prior-attempt lookups at `as_of` (the open time of
    /// the bar the gate is evaluating). Call before each `retry_gate::evaluate`.
    pub fn set_as_of(&self, as_of: DateTime<Utc>) {
        *self.as_of.borrow_mut() = as_of;
    }

    /// Register a placed attempt so a later lookup can resolve it. `order_id`
    /// must match what the gate stored on the `EntryAttempt` (the replay uses
    /// the same id when it `record_placement`s).
    pub fn record_attempt(&self, order_id: String, intent: Intent, shell: Shell) {
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
        // The replay places via simulate_fill in its own loop, not through the
        // broker trait. The gate never calls this.
        unreachable!("ReplayBroker: place_entry is driven by the replay loop, not the gate")
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
        Err(LookupError::Transient)
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
        // The replay feeds candles directly; the gate never fetches them.
        Ok(Vec::new())
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
