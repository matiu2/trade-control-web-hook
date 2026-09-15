//! Matching a broker position back to the [`EntryAttempt`] that opened it.
//!
//! # Why this is shared
//!
//! Two crons needed this and each grew its own copy — `blackout_apply.rs` and
//! `breakeven_watch.rs` held **byte-identical** implementations, aliasing bug
//! and all. Both mutate the *same* stop on the *same* 900s cadence, so a
//! divergence between their joins would mean the two systems disagreeing about
//! which trade a position belongs to while both amending it. One copy, one
//! behaviour.
//!
//! # The two-stage match, and the hazard in stage 2
//!
//! 1. **Exact** — `attempt.broker_trade_id == position.position_id`. Written on
//!    the first sweep tick that observes the attempt has filled, and
//!    unambiguous from then on.
//! 2. **Coarse fallback** — `(instrument, direction, account)`. Covers the
//!    window between the fill and the sweep tick that observes it, plus rows
//!    written before the snapshot existed.
//!
//! ⚠️ **Stage 2 can alias.** Two attempts on the same instrument, same
//! direction, same account — a multi-shot re-entry, or two setups on one pair —
//! are indistinguishable to it, and `find` returns whichever comes first. The
//! consequence is a stop amended against the wrong attempt's geometry.
//!
//! # Stage 1 is now populated for every filled attempt
//!
//! It previously was not, and the shape of the hazard is different enough to be
//! worth stating plainly. The only writer of `broker_trade_id` used to be
//! [`retry_gate`](crate::retry_gate), on the branch where it looks up a prior
//! attempt and **rejects** a re-entry — code an ordinary trade (fills once,
//! never re-fired) never reaches. So the field stayed `None` for the position's
//! whole life, and stage 2 was not the exception but **the only path taken in
//! the common case**, correct purely by luck whenever exactly one position
//! matched.
//!
//! `trade_control_cron::sweep::snapshot_broker_trade_id` now writes it on the
//! first tick an attempt is seen to have filled, so stage 1 carries the ordinary
//! case and stage 2 is the narrow fallback its name implies. Read that function
//! for why the sweep is the seam and what the round-trip costs.
//!
//! **Do not "strengthen" stage 2 with tie-breakers.** It cannot be made correct:
//! the information that separates two look-alike attempts is the broker's own
//! id, and every proxy for it (most recent, nearest stop, first placed) is a
//! guess that will eventually be wrong and silent. Narrowing the window in which
//! stage 2 runs at all is the fix; a cleverer stage 2 is not.

use crate::broker::OpenPosition;
use crate::state::EntryAttempt;

/// The `EntryAttempt` that opened `position`, or `None` if nothing matches.
///
/// See the module docs for the two-stage match and the stage-2 aliasing hazard.
pub fn join_position_to_attempt<'a>(
    position: &OpenPosition,
    account: Option<&str>,
    attempts: &'a [EntryAttempt],
) -> Option<&'a EntryAttempt> {
    // 1. Exact: snapshotted broker_trade_id == position_id.
    if let Some(hit) = attempts
        .iter()
        .find(|a| a.broker_trade_id.as_deref() == Some(position.position_id.as_str()))
    {
        return Some(hit);
    }
    // 2. Coarse fallback: instrument + direction + account. Can alias — see
    //    the module docs.
    attempts.iter().find(|a| {
        a.instrument == position.instrument
            && a.direction == position.direction
            && a.account.as_deref() == account
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::Direction;
    use chrono::{DateTime, Utc};

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    fn attempt(
        trade_id: &str,
        instrument: &str,
        direction: Direction,
        account: Option<&str>,
        broker_trade_id: Option<&str>,
    ) -> EntryAttempt {
        EntryAttempt {
            trade_id: trade_id.into(),
            account: account.map(|s| s.into()),
            instrument: instrument.into(),
            attempt_no: 1,
            broker_order_id: "ord-1".into(),
            broker_trade_id: broker_trade_id.map(|s| s.into()),
            direction,
            placed_at: ts("2026-03-12T20:00:00Z"),
            shell_time: ts("2026-03-12T20:00:00Z"),
            expires_at: ts("2026-03-13T00:00:00Z"),
            stop_loss_price: Some(1.8000),
            adverse_extreme: None,
            cancel_at: None,
            pip_size: Some(0.0001),
            blackout_close: crate::intent::BlackoutCloseAction::default(),
            breakeven: None,
            order_control: None,
            superseded: false,
        }
    }

    fn position(instrument: &str, direction: Direction, position_id: &str) -> OpenPosition {
        OpenPosition {
            instrument: instrument.into(),
            direction,
            stop_loss: Some(1.8000),
            take_profit: None,
            position_id: position_id.into(),
            order_id: "ord-1".into(),
            stake: 1.0,
            // Correlation-only fixtures: this test is about the join keys, so
            // the fill facts are honestly unknown.
            entry_price: None,
            opened_at: None,
        }
    }

    /// Stage 1 wins even when a coarse match sits earlier in the list — the
    /// exact id is the authority.
    #[test]
    fn exact_trade_id_beats_a_coarse_match() {
        let attempts = vec![
            attempt("coarse", "EUR_USD", Direction::Long, None, None),
            attempt("exact", "EUR_USD", Direction::Long, None, Some("pos-9")),
        ];
        let hit = join_position_to_attempt(
            &position("EUR_USD", Direction::Long, "pos-9"),
            None,
            &attempts,
        )
        .expect("a match");
        assert_eq!(hit.trade_id, "exact");
    }

    #[test]
    fn coarse_fallback_matches_on_instrument_direction_account() {
        let attempts = vec![
            attempt("wrong-instrument", "GBP_USD", Direction::Long, None, None),
            attempt("wrong-direction", "EUR_USD", Direction::Short, None, None),
            attempt("right", "EUR_USD", Direction::Long, None, None),
        ];
        let hit = join_position_to_attempt(
            &position("EUR_USD", Direction::Long, "pos-1"),
            None,
            &attempts,
        )
        .expect("a match");
        assert_eq!(hit.trade_id, "right");
    }

    /// Account scoping is part of the coarse key: another account's attempt on
    /// the same instrument and direction must NOT match.
    #[test]
    fn coarse_fallback_respects_account_scope() {
        let attempts = vec![attempt(
            "other-account",
            "EUR_USD",
            Direction::Long,
            Some("acct-b"),
            None,
        )];
        assert!(
            join_position_to_attempt(
                &position("EUR_USD", Direction::Long, "pos-1"),
                Some("acct-a"),
                &attempts,
            )
            .is_none(),
            "an attempt on a different account must not be joined",
        );
    }

    /// Pins the KNOWN aliasing hazard rather than asserting it is correct: two
    /// indistinguishable attempts resolve to the first, so a stop can be amended
    /// against the wrong one's geometry.
    ///
    /// Still reachable, and deliberately so — it is what a position joined
    /// between its fill and the sweep tick that observes the fill falls back to.
    /// What changed is how *often*: with `broker_trade_id` now snapshotted, a
    /// filled attempt leaves this path for good. Both rows here carry `None`,
    /// which after the snapshot lands means neither has been observed to fill
    /// yet.
    #[test]
    fn coarse_fallback_aliases_two_identical_attempts() {
        let attempts = vec![
            attempt("first", "EUR_USD", Direction::Long, None, None),
            attempt("second", "EUR_USD", Direction::Long, None, None),
        ];
        let hit = join_position_to_attempt(
            &position("EUR_USD", Direction::Long, "pos-1"),
            None,
            &attempts,
        )
        .expect("a match");
        assert_eq!(
            hit.trade_id, "first",
            "documented hazard: the coarse key cannot separate a multi-shot \
             re-entry from a second setup on the same pair",
        );
    }

    #[test]
    fn no_match_is_none() {
        let attempts = vec![attempt("other", "GBP_USD", Direction::Long, None, None)];
        assert!(
            join_position_to_attempt(
                &position("EUR_USD", Direction::Long, "pos-1"),
                None,
                &attempts,
            )
            .is_none()
        );
    }
}
