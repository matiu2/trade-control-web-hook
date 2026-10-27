//! Timestamp-aware sequential replay and reproducible Monte Carlo comparisons.
//! R outcomes are net of charges; positions complete before the next record.
mod allocation;
mod monte_carlo;
mod replay;
mod statistics;
mod types;
pub use monte_carlo::{Interval, MonteCarloConfig, MonteCarloReport, monte_carlo};
pub use replay::compare;
pub use types::*;
