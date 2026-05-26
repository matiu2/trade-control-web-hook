//! Shared, broker-agnostic logic for the trade-control web hook.
//!
//! Everything in this crate is pure data + wasm-friendly — no `worker`
//! crate, no broker SDKs, no filesystem. The root crate (the Cloudflare
//! Worker binary) pulls these in and wires them up to the CF-specific bits
//! (KV state store, `#[event(fetch)]` entry point, broker dispatch).

pub mod account;
pub mod broker;
pub mod incoming;
pub mod intent;
pub mod rules;
pub mod sig;
pub mod state;
pub mod tunable;
