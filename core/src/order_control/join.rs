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
//! 1. **Exact** — `attempt.broker_trade_id == position.position_id`. Only set
//!    once the worker has observed the attempt fill, but unambiguous when
//!    present.
//! 2. **Coarse fallback** — `(instrument, direction, account)`. Needed because
//!    stage 1 is blank until the fill is observed, and a cron may run first.
//!
//! ⚠️ **Stage 2 can alias.** Two attempts on the same instrument, same
//! direction, same account — a multi-shot re-entry, or two setups on one pair —
//! are indistinguishable to it, and `find` returns whichever comes first. The
//! consequence is a stop amended against the wrong attempt's geometry.
//!
//! This is inherited behaviour, preserved deliberately: changing the match
//! semantics while merely de-duplicating them would mix a behaviour change into
//! a refactor, on the live money path. It is documented here — rather than in
//! two places, half-noticed — so the fix has one site when it comes. The real
//! fix is to make stage 1 always available (snapshot `broker_trade_id` at
//! placement), not to add tie-breakers to stage 2.

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
            cancel_at: None,
            pip_size: Some(0.0001),
            blackout_close: crate::intent::BlackoutCloseAction::default(),
            breakeven: None,
            order_control: None,
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
    /// against the wrong one's geometry. If a future fix makes this
    /// deterministic by some better rule, this test should be *updated*, not
    /// deleted — it is the record of what the coarse path can't tell apart.
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
