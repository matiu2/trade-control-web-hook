//! Deterministic account-level prop-firm risk controls. No broker I/O in policy calculations.
pub mod account;
pub mod calendar;
pub mod decision;
pub mod exposure;
pub mod ledger;
pub mod manager;
pub mod policy;
pub mod simulation;
pub mod store;
pub use account::*;
pub use decision::*;
pub use exposure::*;
pub use manager::*;
pub use policy::*;
pub use rust_decimal::Decimal;

use color_eyre::eyre::{Result, eyre};

pub(crate) fn mul(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_mul(b)
        .ok_or_else(|| eyre!("risk arithmetic overflow"))
}
pub(crate) fn add(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_add(b)
        .ok_or_else(|| eyre!("risk arithmetic overflow"))
}
pub(crate) fn sub(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_sub(b)
        .ok_or_else(|| eyre!("risk arithmetic overflow"))
}
pub(crate) fn div(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_div(b)
        .ok_or_else(|| eyre!("invalid risk divisor or overflow"))
}
pub(crate) fn pct(reference: Decimal, percent: Decimal) -> Result<Decimal> {
    div(mul(reference, percent)?, Decimal::from(100))
}
