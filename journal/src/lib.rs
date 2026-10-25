//! Shared journal readers and analysis for native clients.
//!
//! The service signs only plan-list, plan-show and plan-timeline requests.
//! Pure parsers are shared with the terminal journal; they never launch a
//! chart, a subprocess, or a trade-management action.

pub mod divergence;
pub mod fixtures;
pub mod plan;
pub mod search;
pub mod service;
pub mod timeline;
