//! One home for stored / pending / live order state.
//!
//! # Vocabulary (load-bearing — the module tree hangs off it)
//!
//! An order is in exactly one of three states:
//!
//! | state | where it lives | at risk? |
//! |---|---|---|
//! | **Stored** | our DB only. Never sent to the broker. | no |
//! | **Pending** | placed with the broker, not yet triggered. | no |
//! | **Live** | filled; it is a position. | **yes** |
//!
//! The three have different **mutation surfaces** and different safety rules,
//! which is why they are the primary split rather than widen-vs-shrink. Widen
//! and shrink are the same question — *"what should this stop be right now?"* —
//! differing only by a sign, so they belong in one pure function with several
//! call sites. Splitting them first is precisely how the two pre-existing widen
//! implementations (`intent::sl_spread_floor` in price units, `blackout_widen`
//! in pips) drifted apart with different constants and different sampling.
//!
//! # Why `core`
//!
//! Everything here is generic over the `Broker` + `StateStore` traits, so the
//! live worker and the offline replay get one implementation and cannot drift
//! (`[[strategy_changes_in_both_replayer_and_worker]]`). It does not belong in
//! `engine`, which is pure — plans and candles in, fires out, no `StateStore`
//! (`[[engine_is_pure_broker_trait_only]]`) — and order control is inherently
//! effectful. Nor in its own crate: it needs `Broker`, `StateStore`, `Intent`
//! and `Holders`, all of which live here, and `dispatch::enter` must call *into*
//! it for the stored path, which would be a dependency cycle.
//!
//! # Sub-modules
//!
//! Each holds a single idea. The pure ones carry the decisions (unit-testable
//! without a broker, and mutation-testable); the effectful ones carry only
//! plumbing.
//!
//! - [`restore`] — **pure.** May a remembered widened stop be given back, or has
//!   another system (break-even) moved it since? Prevents a restore silently
//!   reverting a locked-in break-even.

mod restore;

pub use restore::*;
