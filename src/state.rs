//! Cloudflare KV implementation of the shared [`trade_control_core::state::StateStore`]
//! trait. Lives in the worker crate because it depends on `worker::kv::KvStore`,
//! which is wasm/Workers-specific.

mod kv;

pub use kv::KvStateStore;
