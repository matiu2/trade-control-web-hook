//! Broker-agnostic surface used by the worker dispatch.
//!
//! Each broker crate (`broker-oanda`, future `broker-tradenation`) provides an
//! authenticated handle implementing [`Broker`]. The worker keeps one such
//! handle per request, selected from the encrypted intent's `broker:` field.
//!
//! `?Send` futures match the rest of this codebase: Cloudflare Workers run on a
//! single-threaded executor and broker SDKs hold `!Send` reqwest clients.

use core::future::Future;

use crate::intent::{Direction, ResolvedEntry, RiskBudget};

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
    RiskCapExceeded { requested: f64, cap: f64 },
    OpenPositionsCapExceeded,
    UnitsBelowMinimum,
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

    /// Fetch the current mid-market price for an instrument. Used by the
    /// scheduled SL-breach sweep to decide if a still-pending stop-entry
    /// order has been overtaken by price.
    ///
    /// Returns the mid of bid/ask for FX-style brokers. For brokers
    /// without a quote endpoint, may use the most recent traded /
    /// settlement price — the sweep only needs "good enough to detect a
    /// breach", not tick-accurate execution price.
    fn get_current_price(&self, instrument: &str)
    -> impl Future<Output = Result<f64, LookupError>>;
}
