//! Worker-side wiring for the first-class account system.
//!
//! Two halves:
//!
//! - [`KvMetadataStore`] — Cloudflare KV implementation of
//!   `MetadataStore`. The whole index lives in one key
//!   (`accounts:index`) as a JSON-serialised `Vec<AccountMetadata>`.
//!   Account count is bounded (~20 max — see TODO.md), so a single
//!   key is fine; we avoid the per-account-key sprawl that would
//!   bloat KV listing.
//! - [`SecretCredentialsResolver`] — wasm-only. Reads
//!   `TN_ACCOUNT_<NAME>` / `OANDA_ACCOUNT_<NAME>` secrets and parses
//!   each as a `Credentials` JSON blob.
//!
//! Names from the metadata index are mapped to secret-binding names by
//! [`secret_name_for`]. Operators set secrets via
//! `wrangler secret put TN_ACCOUNT_<NAME>` (or the future
//! `account add` CLI verb that wraps it).

mod kv_metadata;
mod secret_resolver;

pub use kv_metadata::KvMetadataStore;
#[cfg(target_arch = "wasm32")]
pub use secret_resolver::SecretCredentialsResolver;
