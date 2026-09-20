//! Is a session-bound market **shut right now**?
//!
//! The market-hours gate ([`crate::intent::market_hours_blocked`]) knows the
//! weekend and an instrument's *close hours*. It does not know that Spain 35 is
//! shut from 18:00 to 09:00 Madrid. That did not matter while every D1/H4 bar
//! closed on the 17:00 New York grid; it matters now that bars follow the
//! instrument's session anchor, because **a bar that ends at the session close
//! is only closed a full bucket later** — Spain's 15:00Z H4 bar at 19:00Z, its
//! D1 bar at 07:00Z the next day (Saturday, for Friday's bar). An entry fired
//! then would reach a closed market.
//!
//! So `run_enter` parks such an entry ([`StoredReason::MarketClosed`]) and the
//! promote pass places it once [`market_open`] says the session has started.
//! This is the ONE place "reject, never delay" is relaxed, and only for an
//! instrument with a row in the baked session table: an instrument with no row
//! is never "closed" here and keeps the plain reject everywhere.
//!
//! The table ([`SESSION_HOURS_BAKED`]) is candle-derived by `market-hours-gen`
//! per venue, in the market's LOCAL wall clock so one row is right in both DST
//! seasons. Rows exist only for a real overnight closure (≥ 4 closed hours, one
//! contiguous session) — a wrong row would park entries on an open market, so
//! the generator errs toward no row.
//!
//! [`StoredReason::MarketClosed`]: crate::order_control::StoredReason::MarketClosed

use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Tz;

mod baked_table {
    include!("session_hours_baked.rs");
}
use baked_table::SESSION_HOURS_BAKED;

/// A baked local-time session: the market trades `[start_hour, start_hour +
/// hours)` on `tz`'s wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Session {
    tz: Tz,
    start_hour: u32,
    hours: u32,
}

impl Session {
    fn contains(&self, now: DateTime<Utc>) -> bool {
        let local_hour = now.with_timezone(&self.tz).hour();
        (local_hour + 24 - self.start_hour) % 24 < self.hours
    }
}

/// The baked session for a broker-native `symbol`. `None` for an instrument
/// with no row, and — loudly — for a row whose timezone does not parse (the
/// gate then fails open rather than parking on a clock it cannot read).
fn session_of(symbol: &str) -> Option<Session> {
    let (_, _, tz, start_hour, hours) = SESSION_HOURS_BAKED
        .iter()
        .find(|(_, sym, ..)| *sym == symbol)?;
    match tz.parse::<Tz>() {
        Ok(tz) => Some(Session {
            tz,
            start_hour: *start_hour,
            hours: *hours,
        }),
        Err(e) => {
            tracing::error!("session table: {symbol} has an unparseable timezone {tz:?}: {e}");
            None
        }
    }
}

/// Is `symbol` a session-bound market that is shut at `now` — outside its
/// daily session, or inside the universal weekend halt? Always `false` for an
/// instrument with no session row.
pub fn session_closed(symbol: &str, now: DateTime<Utc>) -> bool {
    session_of(symbol).is_some_and(|s| !s.contains(now) || crate::intent::weekend_blocked(now))
}

/// May a [`StoredReason::MarketClosed`](crate::order_control::StoredReason)
/// park be placed at `now`? The session must have started AND the full
/// market-hours mask must be clear, so a promotion never lands in a blocked
/// close hour. An instrument with no session row is always open here (nothing
/// parks it as `MarketClosed` in the first place).
pub fn market_open(symbol: &str, now: DateTime<Utc>) -> bool {
    !session_closed(symbol, now) && !crate::intent::market_hours_blocked(symbol, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("rfc3339")
    }

    /// TradeNation Spain 35: 09:00–18:00 Madrid. 2026-09-08 is a Tuesday, CEST.
    #[test]
    fn spain_35_is_shut_overnight_and_open_in_session() {
        assert!(
            session_closed("Spain 35", at("2026-09-08T19:00:00Z")),
            "the 15:00Z H4 bar's close"
        );
        assert!(session_closed("Spain 35", at("2026-09-08T06:59:00Z")));
        assert!(
            !session_closed("Spain 35", at("2026-09-08T07:00:00Z")),
            "09:00 Madrid: open"
        );
        assert!(!session_closed("Spain 35", at("2026-09-08T15:59:00Z")));
        assert!(
            session_closed("Spain 35", at("2026-09-08T16:00:00Z")),
            "18:00 Madrid: shut"
        );
    }

    /// The row is local time, so winter needs no second row: 09:00 CET = 08:00Z.
    #[test]
    fn the_session_follows_madrid_dst() {
        assert!(session_closed("Spain 35", at("2026-01-13T07:30:00Z")));
        assert!(!session_closed("Spain 35", at("2026-01-13T08:00:00Z")));
    }

    /// Friday's D1 bar closes Saturday 09:00 Madrid — inside the clock span,
    /// but the market is shut for the weekend.
    #[test]
    fn a_session_clock_hour_on_the_weekend_is_still_shut() {
        assert!(
            session_closed("Spain 35", at("2026-09-12T07:00:00Z")),
            "Saturday"
        );
        assert!(!market_open("Spain 35", at("2026-09-12T07:00:00Z")));
        assert!(
            market_open("Spain 35", at("2026-09-14T07:00:00Z")),
            "Monday open"
        );
    }

    /// Release never lands in a blocked close hour (Spain 35: 15:00 and 16:00Z).
    #[test]
    fn market_open_also_respects_the_close_hour_mask() {
        let t = at("2026-09-08T15:30:00Z");
        assert!(!session_closed("Spain 35", t), "still inside the session…");
        assert!(
            !market_open("Spain 35", t),
            "…but the close hour is blocked"
        );
    }

    #[test]
    fn an_instrument_with_no_row_is_never_closed() {
        let midnight = at("2026-09-08T03:00:00Z");
        assert!(!session_closed("EUR/USD", midnight));
        assert!(!session_closed("NO SUCH SYMBOL", midnight));
        // Not even at the weekend: FX keeps the plain market-hours reject.
        assert!(!session_closed("EUR/USD", at("2026-09-12T12:00:00Z")));
    }

    #[test]
    fn a_session_crossing_local_midnight_wraps() {
        // OANDA CORN_USD: 20:00 +19h New York → open 20:00..15:00.
        assert!(
            !session_closed("CORN_USD", at("2026-09-08T04:00:00Z")),
            "00:00 NY"
        );
        assert!(
            session_closed("CORN_USD", at("2026-09-08T20:00:00Z")),
            "16:00 NY"
        );
    }

    #[test]
    fn every_baked_row_is_readable_and_is_a_real_overnight_closure() {
        for (venue, symbol, tz, start, hours) in SESSION_HOURS_BAKED {
            assert!(
                tz.parse::<Tz>().is_ok(),
                "{venue} {symbol}: bad timezone {tz}"
            );
            assert!(*start < 24, "{venue} {symbol}: start {start}");
            assert!(
                (1..=20).contains(hours),
                "{venue} {symbol}: {hours}h is not an overnight closure"
            );
        }
    }
}
