//! The post-placement portion of a parent candle for an intra-bar market retry.
//! Preserve parent-bar management cadence while excluding pre-entry extremes.

use chrono::{DateTime, Utc};
use trade_control_core::broker::BidAskCandle;

pub fn clipped_parent(
    bars: &[BidAskCandle],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    step_seconds: i64,
) -> Option<BidAskCandle> {
    if step_seconds <= 0 {
        return None;
    }
    let selected: Vec<_> = bars
        .iter()
        .filter(|bar| bar.time >= from && bar.time < to)
        .collect();
    let first = **selected.first()?;
    let last = selected.last()?;
    if first.time != from
        || last.time + chrono::Duration::seconds(step_seconds) != to
        || selected
            .windows(2)
            .any(|pair| (pair[1].time - pair[0].time).num_seconds() != step_seconds)
    {
        return None;
    }
    Some(selected.into_iter().skip(1).fold(
        BidAskCandle {
            time: from,
            ..first
        },
        |mut total, bar| {
            total.h = total.h.max(bar.h);
            total.l = total.l.min(bar.l);
            total.c = bar.c;
            total.bid_h = total.bid_h.max(bar.bid_h);
            total.bid_l = total.bid_l.min(bar.bid_l);
            total.bid_c = bar.bid_c;
            total.ask_h = total.ask_h.max(bar.ask_h);
            total.ask_l = total.ask_l.min(bar.ask_l);
            total.ask_c = bar.ask_c;
            total
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn excludes_all_extremes_before_the_retry_quote() {
        use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::from_default_env())
            .with(tracing_error::ErrorLayer::default())
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init()
            .ok();
        let start: DateTime<Utc> = "2026-10-30T01:00:00Z".parse().unwrap();
        let bar = |minute: i64, high, low| BidAskCandle {
            time: start + chrono::Duration::minutes(minute),
            o: 100.0,
            h: high,
            l: low,
            c: 100.0,
            bid_o: 99.9,
            bid_h: high - 0.1,
            bid_l: low - 0.1,
            bid_c: 99.9,
            ask_o: 100.1,
            ask_h: high + 0.1,
            ask_l: low + 0.1,
            ask_c: 100.1,
        };
        let bars = [
            bar(0, 150.0, 50.0),
            bar(1, 101.0, 99.0),
            bar(2, 102.0, 98.0),
        ];
        let clipped = clipped_parent(
            &bars,
            start + chrono::Duration::minutes(1),
            start + chrono::Duration::minutes(3),
            60,
        )
        .unwrap();
        assert_eq!((clipped.h, clipped.l), (102.0, 98.0));
        assert_eq!(clipped.time, start + chrono::Duration::minutes(1));
        assert!(
            clipped_parent(
                &bars[..2],
                start + chrono::Duration::minutes(1),
                start + chrono::Duration::minutes(3),
                60
            )
            .is_none()
        );
        assert!(
            clipped_parent(
                &[bars[0], bars[2]],
                start,
                start + chrono::Duration::minutes(3),
                60
            )
            .is_none()
        );
    }
}
