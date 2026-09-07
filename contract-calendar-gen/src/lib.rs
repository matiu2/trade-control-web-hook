//! Futures **contract close-out calendar** generator.
//!
//! Offline operator tool: derives each listed futures contract month's last
//! trade day, First Notice Day and close-out deadlines from published exchange
//! rules, and renders a committed `contract_calendar_baked.rs` table that
//! `core` `include!`s. The arm-time guard reads that table to refuse plans that
//! would hold a position into its close-out window.
//!
//! # Why this exists
//!
//! IBKR **force-liquidates** an expiring futures position during a close-out
//! period, *without additional prior notice*. There is no broker-side automatic
//! position roll — "Automatic Futures Rollover" is a charting/data-line feature
//! and rolls nothing. So either we retire the position in time, or IBKR
//! flattens it at a price of its choosing.
//!
//! The deadline is **not** the contract's expiry date, and for physically
//! delivered contracts it is roughly a **month** earlier. See [`rules`].
//!
//! # Why arithmetic, not a fetch
//!
//! Unlike its sibling `market-hours-gen` (which measures candle gaps from live
//! broker feeds), every rule here is a deterministic calendar computation over
//! published contract specs. There is no network call, so the whole crate is
//! offline-testable and its output is reproducible.
//!
//! IBKR's own `ContractDetails` reports a last-trade date but exposes **neither
//! settlement type nor First Notice Day**, so the chain cannot answer the
//! question this table answers — which is why the rules are encoded here rather
//! than read off the wire. The live chain is still valuable as *validation*:
//! the `rules` tests pin the derived last-trade days against readings taken
//! from the paper Gateway on 2026-09-06.
//!
//! # Fail closed
//!
//! Every fallible path returns `None`/`Err` rather than a default. An unknown
//! contract, an out-of-range year, or an ambiguous key must reach the caller as
//! a **refusal to arm**, never as "no constraint" — a missing deadline that
//! reads as permission is exactly the failure that gets a position liquidated.

pub mod holiday;
pub mod render;
pub mod rules;

pub use render::{RenderError, contract_month, render_table};
pub use rules::{
    CONTRACT_SPECS, ContractDates, ContractSpec, LastTradeRule, MonthCycle, Settlement,
    all_contract_dates, contract_dates, spec_for,
};
