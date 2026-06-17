//! Server-side trade-plan engine — the cron-driven evaluator that replaces
//! TradingView's paid alerts.
//!
//! # Where this fits
//!
//! Today `tv-arm` reads a hand-drawn chart and creates ~5–15 TradingView
//! alerts per trade; each alert evaluates one condition *on TV's servers* and
//! POSTs a signed [`Intent`](trade_control_core::intent::Intent) to the
//! Cloudflare worker when it fires. This crate inverts that: `tv-arm` will
//! serialise the **whole** trade into one signed `TradePlan`, and this engine —
//! invoked from the worker's existing single cron tick — fetches broker candles
//! itself, evaluates every registered plan, and dispatches the fired intents
//! through the same path the webhook uses. See
//! `~/.home-claude/plans/i-want-to-be-wiggly-squid.md`.
//!
//! # Stage B finding — the shared crate already exists
//!
//! The plan's Stage B asked to "lift the types both the old webhook and the new
//! engine need into a shared crate". On inspection there is **nothing to
//! extract**: [`trade_control_core`] is already that crate. It is `worker`-free,
//! broker-SDK-free, and WASM-buildable, and it already holds every shared type
//! the engine builds on —
//! [`Intent`](trade_control_core::intent::Intent) /
//! [`Action`](trade_control_core::intent::Action),
//! the [`Broker`](trade_control_core::broker::Broker) trait (with the
//! Stage-A [`Candle`](trade_control_core::broker::Candle) /
//! [`Granularity`](trade_control_core::broker::Granularity) /
//! [`get_candles`](trade_control_core::broker::Broker::get_candles) surface),
//! the [`StateStore`](trade_control_core::state::StateStore) trait,
//! signing/verification ([`trade_control_core::sig`]), and the pure
//! `MwState` / `plan_mw_update` evaluator
//! ([`trade_control_core::intent::mw_state`]) that the engine's `evaluate_plan`
//! will generalise.
//!
//! So this crate depends on `trade_control_core` exactly as the worker and the
//! `cli` do. The webhook keeps consuming `core` unchanged; this engine consumes
//! the same `core`; the two coexist with no shared-state coupling beyond the
//! KV [`StateStore`](trade_control_core::state::StateStore) contract. That is
//! the "run both in parallel" guarantee Stage B was meant to deliver.
//!
//! Stage C populates this crate with `TradePlan` + the `register` action;
//! Stage D adds the pure `evaluate_plan` and the cron wiring.

// Re-export the shared surface the engine builds on, so downstream stages (and
// the worker's cron wiring) can name everything through `trade_control_engine`
// without each reaching into `trade_control_core` independently.
pub use trade_control_core::broker::{Broker, Candle, CandleError, Granularity};
pub use trade_control_core::intent::{self, Action, Intent};
pub use trade_control_core::plan_state::{Phase, PlanState};
pub use trade_control_core::state::{StateError, StateStore, StoredPlan};
pub use trade_control_core::trade_plan::{
    BarEvent, ConditionRule, CrossDir, FireMode, LinePoint, TradePlan, Trigger,
};

mod evaluate;
pub use evaluate::{FiredIntent, PlanEval, eval_trigger, evaluate_plan, initial_phase};

#[cfg(test)]
mod tests {
    use super::*;

    /// Stage B smoke test: the engine resolves the shared `core` types it will
    /// build on. If `core`'s boundary regressed (e.g. a `worker`/broker-SDK dep
    /// leaked in and broke the standalone build), this crate would fail to
    /// compile and this test would never run — that compile *is* the assertion.
    #[test]
    fn shared_core_surface_is_reachable() {
        // Naming the Stage-A candle granularity through the engine's re-export
        // proves the dependency edge engine → core is live and the candle
        // surface is part of the shared boundary.
        assert_eq!(Granularity::H1.seconds(), 3600);
    }
}
