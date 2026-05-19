//! First-class account records for routing intents to a specific
//! broker session with per-account risk caps.
//!
//! ## Storage model
//!
//! Credentials are deliberately *not* part of [`AccountMetadata`] — they
//! live behind [`CredentialsResolver`], whose production implementation
//! reads them out of the Cloudflare Worker's Secret Store (one secret
//! binding per account, `TN_ACCOUNT_<NAME>` / `OANDA_ACCOUNT_<NAME>`).
//!
//! Workers KV holds only:
//!
//! 1. An **index** of metadata (name, broker, kind, caps) — readable
//!    without authenticating to the Secret Store, used to enumerate
//!    accounts and answer "which broker / kind?" before paying the
//!    cost of a credential fetch.
//! 2. Cached **session blobs** keyed by account name (out of scope of
//!    this module; the worker continues to own the session cache).
//!
//! This way a KV-only exfil yields no password material — same trust
//! boundary as the worker secrets themselves. See `docs/security.md`
//! (TODO) for the full threat model.
//!
//! ## Layering
//!
//! - [`AccountKind`], [`AccountCaps`], [`AccountMetadata`] — pure data.
//! - [`MetadataStore`] — async trait for the metadata index (CRUD over
//!   `Vec<AccountMetadata>`).
//! - [`CredentialsResolver`] — async trait for the secret-backed
//!   credential lookup. Lives next to the metadata trait so callers can
//!   wire one of each.
//! - [`Credentials`] — broker-tagged credential payload.
//! - [`AccountStore`] — convenience wrapper that bundles a metadata
//!   store + credentials resolver and offers higher-level operations.
//!
//! `MemMetadataStore` and `MemCredentialsResolver` provide in-memory
//! impls for unit tests and the CLI's local-config preview mode.

mod caps;
mod creds;
mod kind;
mod memstore;
mod metadata;
mod store;

pub use caps::AccountCaps;
pub use creds::{Credentials, CredentialsResolver, OandaCreds, TradeNationCreds, TradeNationKind};
pub use kind::AccountKind;
pub use memstore::{MemCredentialsResolver, MemMetadataStore};
pub use metadata::{AccountMetadata, MetadataError, MetadataStore};
pub use store::{AccountStore, AccountStoreError};
