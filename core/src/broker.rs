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
}
