//! Fetch enough actual detector candles, including across closed sessions.
use std::future::Future;

use chrono::{DateTime, Utc};
use trade_control_core::broker::Candle;

const MAX_BACKOFFS: usize = 4;

/// Count unique bars before the earliest fresh candle, so catch-up ticks warm
/// their first signal too. If older history genuinely does not exist, continue
/// normal evaluation with a loud diagnostic: safety/control rules remain live,
/// and incoming bars can warm ATR without replaying guards against future state.
pub(super) async fn fetch_window<F, Fut>(
    mut since: DateTime<Utc>,
    fresh: &[Candle],
    want_history: usize,
    mut fetch: F,
) -> Result<Vec<Candle>, String>
where
    F: FnMut(DateTime<Utc>) -> Fut,
    Fut: Future<Output = Result<Vec<Candle>, String>>,
{
    let earliest = fresh.iter().map(|c| c.time).min();
    for attempt in 0..=MAX_BACKOFFS {
        let mut window = fetch(since).await?;
        window.extend_from_slice(fresh);
        window.sort_by_key(|c| c.time);
        window.dedup_by_key(|c| c.time);
        let history = window.iter().filter(|c| Some(c.time) < earliest).count();
        if history >= want_history {
            return Ok(window);
        }
        if attempt == MAX_BACKOFFS {
            tracing::error!(
                history,
                want_history,
                ?earliest,
                "detector history insufficient after {MAX_BACKOFFS} back-offs; ATR may be \
                 cold, continuing normal evaluation so safety/control rules remain live"
            );
            return Ok(window);
        }
        tracing::warn!(history, want_history, %since, ?earliest, attempt,
            "detector history short across market gaps; widening lookback");
        let earliest = earliest.ok_or("detector history has no fresh candle")?;
        let span = earliest - since;
        if span <= chrono::Duration::zero() {
            return Err("detector history start must precede the fresh candle".into());
        }
        since = since
            .checked_sub_signed(span)
            .ok_or("detector history lookback overflow")?;
    }
    Err("detector history back-off exhausted".into())
}

#[cfg(test)]
mod tests;
