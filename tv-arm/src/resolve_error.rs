//! The outcome type shared by the H&S and M/W trade-spec resolvers.
//!
//! Two genuinely different failures, kept apart because they deserve different
//! treatment at the top of the process:
//!
//! - **`Reject`** — the operator's chart or flags are wrong in a way *they* can
//!   fix (a stale invalidation line, an over-long M/W path, a retracement too
//!   deep). Printed as a plain message and the process exits **1**. No
//!   backtrace, no "error:" prefix — it isn't a bug, it's an answer.
//! - **`Fatal`** — an internal failure (a broker read that died, a key that
//!   won't load). Propagates as a normal `eyre` error with its chain intact.
//!
//! Collapsing the two would be a real loss in both directions: a rejection
//! dressed as an error sends the operator hunting for a bug that isn't there,
//! and an internal error dressed as a rejection tells them to redraw a chart
//! that was fine.
//!
//! The `From<eyre::Error>` impl means `?` inside a resolver defaults to
//! `Fatal` — which is the safe default, since a rejection is always written
//! deliberately.

/// Outcome of trade-spec resolution. See the module doc.
#[derive(Debug)]
pub enum ResolveError {
    /// Operator-facing "fix your chart / flags" message. Printed; exit 1.
    Reject(String),
    /// Internal failure; propagates as an error.
    Fatal(color_eyre::eyre::Error),
}

impl From<color_eyre::eyre::Error> for ResolveError {
    fn from(e: color_eyre::eyre::Error) -> Self {
        ResolveError::Fatal(e)
    }
}
