//! Re-export of the TradeNationAdapter, now extracted to the shared
//! `broker-tradenation-adapter` crate so the native runtime can use it too.
//! Kept as a module path alias so existing `crate::tradenation_adapter::*`
//! references compile unchanged.
pub use broker_tradenation_adapter::TradeNationAdapter;
