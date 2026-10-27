//! Shared journal readers and analysis for native clients.
//!
//! The service signs journal reads and explicit plan administration requests.
//! Pure parsers are shared with the terminal journal; they never launch a
//! chart, a subprocess, or a trade-management action.

pub mod divergence;
pub mod fixtures;
pub mod plan;
pub mod plan_admin;
pub mod search;
pub mod service;
pub mod timeline;
