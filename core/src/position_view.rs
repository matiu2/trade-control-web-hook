//! The [`PositionView`] seam: how the broker-free engine learns whether a
//! position is actually open when deciding if a `06-close-on-reversal` should
//! retire the plan's spine.
//!
//! ## Why this exists
//!
//! A reversal-close is "flatten the position **if** one is open". Whether it
//! retires the plan therefore depends on whether a position is open — but the
//! engine ([`evaluate_plan`](../engine)) is deliberately **pure and
//! broker-free** (it is shared, unchanged, by the live worker cron and the
//! offline `replay-candles` sim). Historically that forced a chain of *proxies*
//! for "is a position open?" — always-terminal, then window-shape, then
//! `entry_fired_at` — each with a blind spot that re-opened the same bug (see
//! the `reversal_close_spine_retire_recurring` project memory).
//!
//! `PositionView` replaces the proxy chain with **ground truth injected at the
//! seam**: the engine calls [`PositionView::is_open`], the worker answers from
//! the broker's live positions, and the replay **fakes** it from its own fill
//! simulation. The engine stays pure and synchronous; the async / stateful part
//! lives in each caller's impl.
//!
//! ## What it can honestly answer
//!
//! The broker's `OpenPosition` carries no `trade_id` — only `instrument`,
//! `direction`, and broker ids. So the honest question is **"is a position open
//! on this instrument, in this direction?"**, not "is *this exact trade* open".
//! That is enough for the terminate decision: a reversal-close for a long trade
//! wants to know whether our long on this instrument is still live, and the
//! engine already knows `plan.direction`.

use crate::intent::Direction;

/// Answers whether a position is open on an instrument in a given direction, for
/// the engine's reversal-close terminate decision. See the module docs.
///
/// Implementations:
/// - **worker** — snapshots `Broker::list_open_positions` once per tick (only
///   for plans that carry a reversal-close), filtered to the instrument.
/// - **replay** — reflects the sim's fill state as-of the replay cursor.
/// - [`NoPositions`] — the flat-defaulting stub: everything is closed. Used as
///   the safe default (a close over a flat book is a logged no-op that leaves
///   the spine alive) and in tests that don't exercise an open position.
pub trait PositionView {
    /// Is a position open on `instrument` in `direction`?
    fn is_open(&self, instrument: &str, direction: Direction) -> bool;
}

/// A [`PositionView`] that reports **everything closed**. This is the fail-safe
/// default: with no position, a reversal-close is non-terminal (it dispatches a
/// harmless flatten and the pending enter keeps its window). Using this where a
/// real view isn't wired means "never terminate on a reversal-close" — the
/// conservative direction, since a missed terminate just re-checks next tick
/// while a wrong terminate archives the plan irrecoverably.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPositions;

impl PositionView for NoPositions {
    fn is_open(&self, _instrument: &str, _direction: Direction) -> bool {
        false
    }
}

/// A [`PositionView`] backed by a plain list of open `(instrument, direction)`
/// pairs — the shape both the worker (from `list_open_positions`) and the replay
/// (from sim fills) reduce to. Instrument match is exact; direction must match.
#[derive(Debug, Clone, Default)]
pub struct OpenSet {
    open: Vec<(String, Direction)>,
}

impl OpenSet {
    /// Build from `(instrument, direction)` pairs. Callers filter/​normalise
    /// instrument names to the plan's form before constructing.
    pub fn new(open: Vec<(String, Direction)>) -> Self {
        Self { open }
    }

    /// True if nothing is open.
    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }
}

impl PositionView for OpenSet {
    fn is_open(&self, instrument: &str, direction: Direction) -> bool {
        self.open
            .iter()
            .any(|(i, d)| i == instrument && *d == direction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_positions_is_always_closed() {
        let v = NoPositions;
        assert!(!v.is_open("EUR_USD", Direction::Long));
        assert!(!v.is_open("EUR_USD", Direction::Short));
    }

    #[test]
    fn open_set_matches_instrument_and_direction() {
        let v = OpenSet::new(vec![("EUR_USD".into(), Direction::Long)]);
        assert!(v.is_open("EUR_USD", Direction::Long));
        // wrong direction on the same instrument → not our position
        assert!(!v.is_open("EUR_USD", Direction::Short));
        // different instrument
        assert!(!v.is_open("GBP_USD", Direction::Long));
    }

    #[test]
    fn open_set_empty_is_closed() {
        let v = OpenSet::default();
        assert!(v.is_empty());
        assert!(!v.is_open("EUR_USD", Direction::Long));
    }
}
