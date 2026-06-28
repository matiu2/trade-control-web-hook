//! Shared, broker-agnostic logic for the trade-control web hook.
//!
//! Everything in this crate is pure data + wasm-friendly — no `worker`
//! crate, no broker SDKs, no filesystem. The root crate (the Cloudflare
//! Worker binary) pulls these in and wires them up to the CF-specific bits
//! (KV state store, `#[event(fetch)]` entry point, broker dispatch).

pub mod account;
pub mod allow_close_gate;
pub mod allow_entry_gate;
pub mod blackout_recreate;
pub mod blackout_widen;
pub mod broker;
pub mod candle_gate;
pub mod control_event;
pub mod incoming;
pub mod intent;
pub mod ny_clock;
pub mod pause_gate;
pub mod plan_eval;
pub mod plan_state;
pub mod recover_entry;
pub mod retry_gate;
pub mod rules;
pub mod sig;
pub mod signals;
pub mod spread_blackout;
pub mod state;
pub mod sweep_gate;
pub mod tick_bundle;
pub mod trade_plan;
pub mod tunable;
