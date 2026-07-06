//! Broker-agnostic surface used by the worker dispatch.
//!
//! Each broker crate (`broker-oanda`, future `broker-tradenation`) provides an
//! authenticated handle implementing [`Broker`]. The worker keeps one such
//! handle per request, selected from the encrypted intent's `broker:` field.
//!
//! `?Send` futures match the rest of this codebase: Cloudflare Workers run on a
//! single-threaded executor and broker SDKs hold `!Send` reqwest clients.

use core::future::Future;

use chrono::{DateTime, Utc};

use crate::intent::{Direction, ResolvedEntry, RiskBudget};

mod candles;
pub use candles::*;

/// A live two-sided quote. `spread()` is the spread-blackout feature's
/// filter signal; everything else in the codebase only needs `mid()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quote {
    pub bid: f64,
    pub ask: f64,
}

impl Quote {
    /// Mid-market price — the historic `get_current_price` value.
    pub fn mid(&self) -> f64 {
        (self.bid + self.ask) / 2.0
    }

    /// Ask minus bid. The blackout systems reject / pause on a wide spread.
    pub fn spread(&self) -> f64 {
        self.ask - self.bid
    }
}

/// An open position as the broker reports it. Broker ids are `String`s
/// (trait-wide convention — TradeNation's `u64`s are formatted on the way
/// out and parsed back inside the impl).
///
/// `order_id` is the **originating** order id (TradeNation's amend key);
/// `position_id` is the distinct position / trade id. On OANDA the two are
/// the same trade id (there is no separate originating-order concept once a
/// trade is open), so both are set to the trade id.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenPosition {
    /// TradeNation `market_name` / OANDA `instrument`.
    pub instrument: String,
    pub direction: Direction,
    /// The stop-loss price, if one is attached.
    pub stop_loss: Option<f64>,
    /// The take-profit price, if one is attached.
    pub take_profit: Option<f64>,
    /// TradeNation `PositionID` / OANDA trade id.
    pub position_id: String,
    /// TradeNation originating `OrderID` (the amend key) / OANDA trade id.
    pub order_id: String,
    /// TradeNation stake / OANDA units.
    pub stake: f64,
}

/// A resting (unfilled) entry order. `trigger` is the entry price; `is_stop`
/// is `true` for a stop-entry, `false` for a limit-entry.
///
/// **Gotcha (TradeNation):** the `trigger` here is the entry trigger, **not**
/// the attached stop-loss / take-profit. TradeNation reports an opening
/// order's entry trigger in `stop_order_price` / `limit_order_price`
/// (whichever side is set); the order's real SL/TP live in separate raw
/// `IDO*` fields that the upstream struct does not currently parse. Do not
/// mislabel `trigger` as a stop-loss when consuming this in later sub-plans.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingOrder {
    pub order_id: String,
    pub instrument: String,
    pub direction: Direction,
    pub trigger: f64,
    pub is_stop: bool,
    pub stake: f64,
}

/// Failure modes for [`Broker::amend_stop`]. Same playbook as
/// [`CancelError`]: `Transient` means "re-list and decide", it is not a
/// signal to retry the amend blindly.
#[derive(Debug, Clone, PartialEq)]
pub enum AmendError {
    /// Network / 5xx / other transient broker failure. The caller should
    /// re-list open positions / pending orders and decide from there.
    Transient,
    /// The id wasn't found among open positions or pending orders.
    NotFound,
}

impl core::fmt::Display for AmendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Transient => f.write_str("broker amend failed (transient)"),
            Self::NotFound => f.write_str("no open position or pending order with that id"),
        }
    }
}

impl std::error::Error for AmendError {}

/// Inputs for placing an entry order. Borrowed because it is built per-request
/// from the resolved intent and never outlives the dispatch frame.
pub struct EntryRequest<'a> {
    pub instrument: &'a str,
    pub direction: Direction,
    pub entry: ResolvedEntry,
    pub stop_loss: f64,
    pub take_profit: f64,
    /// How much equity to commit. `Percent` is the historic mode;
    /// `Amount` is a fixed money sum in account currency.
    pub risk: RiskBudget,
    /// When `true`, the broker runs the full sizing path (account
    /// fetch, FX, units calc) and logs the result but does **not**
    /// place the order. Returns a synthetic order id like `"dry-run"`
    /// so callers can treat it as success.
    pub dry_run: bool,
}

/// Failure modes for [`Broker::place_entry`]. Brokers map their own error
/// shapes onto these variants so the worker can render a uniform response.
#[derive(Debug)]
pub enum EntryError {
    AccountFetch,
    EquityParse,
    RiskCapExceeded {
        requested: f64,
        cap: f64,
    },
    OpenPositionsCapExceeded,
    UnitsBelowMinimum,
    /// The entry trigger is on the wrong side of the market (a buy-stop
    /// resting below price / sell-stop above, or the limit analogue).
    /// On TradeNation this is `#19-10` / `#19-9` (`d.Status == -19`).
    /// Distinct from [`Self::OrderRejected`] so the worker's
    /// `recover_entry` fallback can recover (re-place as market / limit,
    /// or skip) instead of dropping the entry. The stop-loss distance is
    /// **not** the cause.
    EntryTooCloseToMarket,
    OrderRejected,
}

impl core::fmt::Display for EntryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AccountFetch => f.write_str("failed to fetch account"),
            Self::EquityParse => f.write_str("failed to parse account equity"),
            Self::RiskCapExceeded { requested, cap } => {
                write!(f, "risk {requested}% > cap {cap}%")
            }
            Self::OpenPositionsCapExceeded => f.write_str("open positions cap exceeded"),
            Self::UnitsBelowMinimum => f.write_str("computed units below minimum"),
            Self::EntryTooCloseToMarket => {
                f.write_str("entry trigger too close to (or wrong side of) the market price")
            }
            Self::OrderRejected => f.write_str("broker rejected the order"),
        }
    }
}

impl std::error::Error for EntryError {}

/// State of a previously-placed entry attempt, as observed at the
/// broker. Returned by [`Broker::lookup_attempt_state`].
///
/// See plan B (`max_retries`) for the algorithm: the worker walks the
/// list of `EntryAttempt` rows for a `(account, trade_id)` group and
/// asks the broker about each prior attempt's outcome. The newest
/// attempt dominates — the worker stops at the first attempt whose
/// state blocks a fresh placement.
#[derive(Debug, Clone, PartialEq)]
pub enum AttemptState {
    /// Resting stop/limit order, not yet filled.
    Pending,
    /// Filled, position still open. `broker_trade_id` is the upstream
    /// `BrokerTrade.id`; the caller snapshots it onto the EntryAttempt
    /// row so the next (post-close) lookup can correlate via
    /// `get_closed_trades`.
    OpenPosition { broker_trade_id: String },
    /// Filled, position closed at a profit. Only resolvable if we
    /// snapshotted `broker_trade_id` while it was open and it still
    /// appears in the closed-trades window.
    ClosedWin { realized_pl: f64 },
    /// Filled, position closed at a loss (or breakeven).
    ClosedLossOrBreakeven { realized_pl: f64 },
    /// Order vanished from pending without filling
    /// (rejected/expired/cancelled). TradeNation can do this upstream;
    /// OANDA can via TIF expiry. Treat as "this attempt is dead;
    /// the next entry message can try again".
    Cancelled,
    /// Order not found anywhere — neither pending nor open, and
    /// either never had a snapshotted trade id or its closed-trade
    /// row aged out. Equivalent to `Cancelled` for retry-counting,
    /// but logged distinctly since it means we lost track.
    Unknown,
}

/// Failure modes for [`Broker::lookup_attempt_state`].
#[derive(Debug, Clone, PartialEq)]
pub enum LookupError {
    /// Network / 5xx / other transient broker failure. The caller
    /// should reject the current fire (no order placed) and let the
    /// next arrival retry.
    Transient,
}

impl core::fmt::Display for LookupError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Transient => f.write_str("broker lookup failed (transient)"),
        }
    }
}

impl std::error::Error for LookupError {}

/// Failure modes for [`Broker::cancel_order`]. Modelled the same way
/// as [`LookupError`]: the caller treats `Transient` as "the order
/// may or may not be cancelled — re-run `lookup_attempt_state` and
/// decide from there" rather than retrying the cancel itself.
#[derive(Debug, Clone, PartialEq)]
pub enum CancelError {
    /// Network / 5xx / order-already-gone / other transient failure.
    /// The plan's race-handling note: between observing `Pending` and
    /// issuing the cancel, the order can fill — treat the cancel
    /// failure as "probably filled" and re-lookup.
    Transient,
}

impl core::fmt::Display for CancelError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Transient => f.write_str("broker cancel failed (transient)"),
        }
    }
}

impl std::error::Error for CancelError {}

/// Authenticated broker handle. The constructor lives on each implementation
/// (it depends on broker-specific secrets), so the trait only carries actions.
pub trait Broker {
    /// Risk-gate + place an entry order. Returns a broker-specific order id.
    fn place_entry(
        &self,
        max_risk_pct: f64,
        max_open_positions: u32,
        req: &EntryRequest<'_>,
    ) -> impl Future<Output = Result<String, EntryError>>;

    /// Close all positions for `instrument`. Returns true if anything closed.
    fn close_positions(&self, instrument: &str) -> impl Future<Output = bool>;

    /// Cancel pending orders on `instrument`. Returns the number cancelled.
    fn cancel_pending_for_instrument(&self, instrument: &str) -> impl Future<Output = usize>;

    /// Look up an attempt this worker previously placed. Used by the
    /// `max_retries` retry gate to ask "is this order still resting,
    /// filled, or closed?".
    ///
    /// `broker_order_id` is what [`Broker::place_entry`] returned;
    /// `broker_trade_id` is the upstream `BrokerTrade.id` if the
    /// worker has already snapshotted it onto the
    /// `EntryAttempt` row (it does so on the first lookup that
    /// finds the attempt as an open trade).
    ///
    /// Algorithm (same on both brokers):
    /// 1. If any pending order has `id == broker_order_id` → `Pending`.
    /// 2. If any open trade has `originating_order_id ==
    ///    Some(broker_order_id)` → `OpenPosition { broker_trade_id:
    ///    trade.id }`. **Match via `originating_order_id`, not
    ///    `trade.id` — on TradeNation those differ (PositionID vs
    ///    OrderID).**
    /// 3. Otherwise, if we have a stored `broker_trade_id`, look it
    ///    up in `get_closed_trades` and bucket realised P&L:
    ///    `> 0` → `ClosedWin`, `<= 0` → `ClosedLossOrBreakeven`.
    /// 4. Otherwise → `Cancelled` (or `Unknown` if we never
    ///    snapshotted a trade id).
    fn lookup_attempt_state(
        &self,
        instrument: &str,
        broker_order_id: &str,
        broker_trade_id: Option<&str>,
    ) -> impl Future<Output = Result<AttemptState, LookupError>>;

    /// Cancel a specific pending order by broker id. Used by the
    /// `max_retries` retry gate's "replace pending" branch — when a
    /// new entry message arrives and the previous attempt's stop /
    /// limit is still resting, we cancel it then place the
    /// replacement.
    ///
    /// The plan's race-handling note: between observing `Pending`
    /// from `lookup_attempt_state` and issuing this cancel, the
    /// order can fill. A `CancelError::Transient` is treated as
    /// "probably filled, re-lookup" rather than as something to
    /// retry.
    fn cancel_order(
        &self,
        account_id: &str,
        broker_order_id: &str,
    ) -> impl Future<Output = Result<(), CancelError>>;

    /// Fetch a live two-sided [`Quote`] for an instrument. The
    /// spread-blackout systems read [`Quote::spread`]; everything else
    /// reads [`Quote::mid`] via the [`Broker::get_current_price`] default.
    fn get_quote(&self, instrument: &str) -> impl Future<Output = Result<Quote, LookupError>>;

    /// All open positions on the account.
    ///
    /// `account_id` follows the [`Broker::cancel_order`] convention — impls
    /// may ignore it (TradeNation binds the account via the session, OANDA
    /// at construction time).
    fn list_open_positions(
        &self,
        account_id: &str,
    ) -> impl Future<Output = Result<Vec<OpenPosition>, LookupError>>;

    /// Move the stop-loss on an open position (or a pending order's SL) to
    /// `new_stop`. Take-profit, entry trigger, and stake are left untouched.
    ///
    /// `position_or_order_id` is matched against open positions first, then
    /// pending orders; an unmatched id yields [`AmendError::NotFound`].
    ///
    /// **UNVERIFIED on TradeNation:** the upstream `amend_order`
    /// (`AmendCloseOrder`) has zero callers and it is not yet confirmed that
    /// it amends an **open position's** SL (keyed by the position's
    /// originating order id) rather than only a resting entry order's SL.
    /// Sub-plan 4 must demo-confirm this on the `reversals` account before
    /// any live stop-widening relies on it. See CHANGELOG v18 / TODO.md.
    fn amend_stop(
        &self,
        account_id: &str,
        position_or_order_id: &str,
        new_stop: f64,
    ) -> impl Future<Output = Result<(), AmendError>>;

    /// All resting (unfilled) entry orders on the account. `account_id` is
    /// ignored by both impls (see [`Broker::list_open_positions`]).
    fn list_pending_orders(
        &self,
        account_id: &str,
    ) -> impl Future<Output = Result<Vec<PendingOrder>, LookupError>>;

    /// Fetch the current mid-market price for an instrument. Used by the
    /// scheduled SL-breach sweep to decide if a still-pending stop-entry
    /// order has been overtaken by price.
    ///
    /// Default implementation = [`Quote::mid`] over [`Broker::get_quote`],
    /// so the mid logic lives in exactly one place. Brokers without a true
    /// quote endpoint override `get_quote` to use the most recent traded /
    /// settlement price — the sweep only needs "good enough to detect a
    /// breach", not tick-accurate execution price.
    fn get_current_price(
        &self,
        instrument: &str,
    ) -> impl Future<Output = Result<f64, LookupError>> {
        async move { Ok(self.get_quote(instrument).await?.mid()) }
    }

    /// Fetch **closed** MID candles for `instrument` at `granularity` whose
    /// open-time falls in `(since, now]`. The trade-plan engine calls this each
    /// cron tick to replay whatever closed since its per-plan watermark.
    ///
    /// Contract (impls must honour all four):
    /// - **Closed only** — the still-forming current bar is dropped.
    /// - **Strictly after `since`** — a candle whose `time == since` was
    ///   already processed; impls run results through
    ///   [`filter_new_candles`] (or equivalent) before returning.
    /// - **Ascending by `time`** — oldest first.
    /// - **MID prices** — not bid/ask.
    ///
    /// A degenerate window (`since >= now`) yields [`CandleError::BadRange`];
    /// a feed failure yields [`CandleError::Transient`] (skip this tick).
    fn get_candles(
        &self,
        instrument: &str,
        granularity: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> impl Future<Output = Result<Vec<Candle>, CandleError>>;

    /// Fetch **closed** candles carrying **both** mid and the broker's per-bar
    /// bid/ask books ([`BidAskCandle`]), same `(since, now]` windowing contract
    /// as [`Broker::get_candles`] (closed-only, strictly-after-`since`,
    /// ascending). The extra books let a caller read the real spread bar-by-bar
    /// — the [`entry SL-spread floor`](crate::intent::mean_spread) averages
    /// `ask_c − bid_c` over the last N to avoid sizing off one spiky bar.
    ///
    /// **Default impl returns [`CandleError::Transient`]** so a broker that
    /// hasn't implemented a two-sided candle feed compiles unchanged and its
    /// callers **fail open** (the SL-floor caller falls back to its single
    /// live-quote path — the pre-window behaviour). A real impl (OANDA keeps the
    /// `MBA` bid/ask it already fetches; TradeNation fetches bid+ask) overrides
    /// this. Distinct from `get_candles` so the engine's mid-only evaluation
    /// path (which must stay mid) is untouched.
    fn get_bidask_candles(
        &self,
        _instrument: &str,
        _granularity: Granularity,
        _since: DateTime<Utc>,
        _now: DateTime<Utc>,
    ) -> impl Future<Output = Result<Vec<BidAskCandle>, CandleError>> {
        async { Err(CandleError::Transient) }
    }
}

#[cfg(test)]
mod quote_tests {
    use super::*;

    #[test]
    fn mid_is_half_sum() {
        let q = Quote {
            bid: 1.1000,
            ask: 1.1004,
        };
        assert!((q.mid() - 1.1002).abs() < 1e-12);
    }

    #[test]
    fn spread_is_ask_minus_bid() {
        let q = Quote {
            bid: 1.1000,
            ask: 1.1004,
        };
        assert!((q.spread() - 0.0004).abs() < 1e-12);
    }

    /// A tiny broker implementing only `get_quote`, proving the default
    /// `get_current_price` returns the quote's mid.
    struct MidOnlyBroker {
        bid: f64,
        ask: f64,
    }

    impl Broker for MidOnlyBroker {
        async fn place_entry(
            &self,
            _max_risk_pct: f64,
            _max_open_positions: u32,
            _req: &EntryRequest<'_>,
        ) -> Result<String, EntryError> {
            Ok("noop".into())
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
            _broker_order_id: &str,
            _broker_trade_id: Option<&str>,
        ) -> Result<AttemptState, LookupError> {
            Ok(AttemptState::Unknown)
        }
        async fn cancel_order(
            &self,
            _account_id: &str,
            _broker_order_id: &str,
        ) -> Result<(), CancelError> {
            Ok(())
        }
        async fn get_quote(&self, _instrument: &str) -> Result<Quote, LookupError> {
            Ok(Quote {
                bid: self.bid,
                ask: self.ask,
            })
        }
        async fn list_open_positions(
            &self,
            _account_id: &str,
        ) -> Result<Vec<OpenPosition>, LookupError> {
            Ok(vec![])
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
            Ok(vec![])
        }
        async fn get_candles(
            &self,
            _instrument: &str,
            _granularity: Granularity,
            _since: DateTime<Utc>,
            _now: DateTime<Utc>,
        ) -> Result<Vec<Candle>, CandleError> {
            Ok(vec![])
        }
    }

    #[test]
    fn default_current_price_is_mid_over_quote() {
        let broker = MidOnlyBroker {
            bid: 1.2500,
            ask: 1.2510,
        };
        // Poll the future to completion on a trivial executor.
        let price = pollster_block(broker.get_current_price("EUR_USD"));
        assert!((price.expect("price") - 1.2505).abs() < 1e-12);
    }

    /// Minimal block-on for `?Send` futures without pulling in a runtime.
    fn pollster_block<F: Future>(fut: F) -> F::Output {
        use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn raw() -> RawWaker {
            fn no_op(_: *const ()) {}
            fn clone(_: *const ()) -> RawWaker {
                raw()
            }
            RawWaker::new(
                core::ptr::null(),
                &RawWakerVTable::new(clone, no_op, no_op, no_op),
            )
        }
        let waker = unsafe { Waker::from_raw(raw()) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = core::pin::pin!(fut);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }
}
