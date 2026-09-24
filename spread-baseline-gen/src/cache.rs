//! Cached M1 bid/ask fetch via `candle-cache`.
//!
//! Replaces the two hand-rolled broker paths ([`crate::fetch::fetch_oanda_minutes`]
//! and the TradeNation adapter's paged `get_bidask_candles`) with one
//! cache-backed call, so a re-run of the generator is served from the warm
//! per-broker table instead of re-asking the broker for 90 days of minutes.
//!
//! Three properties matter here, and each is a reason this is not just a
//! refactor:
//!
//! 1. **The run becomes resumable.** A 90-day M1 window across the whole
//!    catalog is ~130k bars per instrument. `get_candles_range_bid_ask` does
//!    gap analysis and only fetches the *missing* sub-ranges, so a run
//!    interrupted by a rate limit picks up where it stopped rather than
//!    starting over.
//! 2. **Rate limits are handled, not hit.** The cache classifies broker errors
//!    and backs off exponentially on `RateLimited`, and bounds concurrent
//!    broker calls with a semaphore. The old paths had neither.
//! 3. **It populates the shared cache.** The table name derives from the cache
//!    dir's final path component, so pointing at the same per-broker dirs the
//!    strategy crates use (`~/.cache/candle_cache_{oanda,tradenation}`) means
//!    this run's minutes are there for every later replay — and vice versa.
//!
//! Cache-dir choice is deliberately the same rule the replay uses
//! (`cli/src/bin/replay_candles/candles.rs`): per-broker, never one shared
//! table, because cache keys embed each broker's own symbol spelling.
//!
//! ## A warm run still makes a handful of broker calls — that is not a bug
//!
//! Measured on OANDA `EUR_USD`, `--days 7`: the first run fetched ~7,100
//! candles in 3 requests; repeat runs converge to a steady **5 requests and
//! ~71% coverage**, and the computed mask is identical (`[17]`) throughout.
//!
//! The residual gaps are the minutes the broker has **no bars for** — the
//! 20:59–21:15 daily rollover break and the whole Friday-close→Sunday-open
//! weekend. An empty answer caches only as a known-empty marker for *settled*
//! buckets, so a window that keeps sliding forward with `Utc::now()` keeps
//! re-asking about the same dead time. It is bounded and stable, not a leak:
//! the count converges and does not grow with repetition.
//!
//! So "coverage < 100%" on an FX instrument is the expected steady state. What
//! would signal a real problem is coverage near 0% on a re-run, or a request
//! count that climbs run over run.

use std::path::PathBuf;

use candle_cache::{CacheClient, CacheConfig};
use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, WrapErr, eyre};

use crate::Broker;
use crate::compute::MinuteBar;

/// The per-broker cache location, matching what the strategy crates and the
/// replay both use (`~/.cache/candle_cache_{oanda,tradenation}`).
///
/// Two reasons this must be per-broker rather than one shared default, both
/// carried over from the replay's version of this function:
///
/// 1. **It's what makes the cache warm.** Under `postgres-storage` the *table
///    name* derives from this path's final component, so a default
///    (`./candle_cache`) would write to a small private table while the large
///    TradeNation and OANDA tables sit unused beside it.
/// 2. **It prevents cross-broker collision.** Cache keys embed the broker's own
///    symbol formatting (`ba_EUR/USD_M1_…` TradeNation vs `ba_EUR_USD_M1_…`
///    OANDA), so one shared table is only correct while no two brokers spell an
///    instrument identically — true today by luck, not design.
pub fn default_cache_dir(broker: Broker) -> PathBuf {
    // With HOME unset this falls back to the cwd, which under postgres-storage
    // silently changes the TABLE NAME with the directory the command is run
    // from — a cold cache with no obvious cause. Warn rather than degrade
    // silently; the path is unchanged so nothing that worked breaks.
    let base = match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".cache"),
        Err(_) => {
            tracing::warn!(
                "HOME is not set — falling back to a cwd-relative candle cache. Under \
                 postgres-storage the table name comes from this path, so this run will \
                 NOT share the warm per-broker table and will re-fetch everything."
            );
            PathBuf::from(".")
        }
    };
    match broker {
        Broker::Oanda => base.join("candle_cache_oanda"),
        Broker::TradeNation => base.join("candle_cache_tradenation"),
    }
}

/// Normalize candle-cache bid/ask candles to [`MinuteBar`]s, stamping each with
/// its schedule-LOCAL hour via `tz`.
///
/// The sibling of [`crate::fetch::minutes_from_bidask`] for candle-cache's own
/// candle type; both delegate to the same `minute_bar_from_closes` reducer, so
/// the degenerate-bar rules (non-positive mid, inverted spread) and the
/// DST-invariant local-hour stamping are shared, not re-implemented.
pub fn minutes_from_cache(
    candles: &[candle_model::BidAskCandleData],
    tz: chrono_tz::Tz,
) -> Vec<MinuteBar> {
    candles
        .iter()
        .filter_map(|c| {
            crate::fetch::minute_bar_from_closes(
                c.timestamp.with_timezone(&Utc),
                tz,
                c.close,
                c.bid_close,
                c.ask_close,
            )
        })
        .collect()
}

/// A per-broker cached M1 source, built once and reused for every instrument.
///
/// Held for the whole run so the underlying connection pool and the broker-call
/// semaphore are shared across instruments — which is what actually bounds
/// concurrent broker calls and makes the backoff effective.
pub enum CachedSource {
    Oanda(Box<CacheClient<oanda_client::data_source::OandaDataSource>>),
    TradeNation(Box<CacheClient<tradenation_api::TradeNationClient>>),
}

impl CachedSource {
    /// Build the cache-backed source for `broker`, using `cache_dir` (or the
    /// per-broker default).
    pub async fn open(broker: Broker, cache_dir: Option<PathBuf>) -> Result<Self> {
        let config = CacheConfig::default()
            .with_cache_dir(cache_dir.unwrap_or_else(|| default_cache_dir(broker)));
        match broker {
            Broker::Oanda => {
                let client = CacheClient::new(config, oanda_source()?)
                    .await
                    .map_err(|e| cache_unreachable(&e.to_string()))?;
                Ok(Self::Oanda(Box::new(client)))
            }
            Broker::TradeNation => {
                let client = CacheClient::new(config, tradenation_source()?)
                    .await
                    .map_err(|e| cache_unreachable(&e.to_string()))?;
                Ok(Self::TradeNation(Box::new(client)))
            }
        }
    }

    /// Fetch `[from, to]` M1 bid/ask candles for `symbol` through the cache and
    /// reduce them to [`MinuteBar`]s in the asset's schedule-local hour.
    pub async fn minutes(
        &self,
        symbol: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        tz: chrono_tz::Tz,
    ) -> Result<Vec<MinuteBar>> {
        use candle_model::Granularity;

        let (from_fx, to_fx) = (from.fixed_offset(), to.fixed_offset());
        let candles = match self {
            Self::Oanda(c) => c
                .get_candles_range_bid_ask(symbol, from_fx, to_fx, Granularity::OneMinute)
                .await
                .wrap_err_with(|| format!("cached OANDA M1 fetch for {symbol}"))?,
            Self::TradeNation(c) => c
                .get_candles_range_bid_ask(symbol, from_fx, to_fx, Granularity::OneMinute)
                .await
                .wrap_err_with(|| format!("cached TradeNation M1 fetch for {symbol}"))?,
        };
        Ok(minutes_from_cache(&candles.candles, tz))
    }
}

/// A cache that can't be opened is fatal, never a silent degrade.
///
/// An unreachable database or a bad `DATABASE_URL` must not be mistaken for a
/// run that simply found no data: a mask computed from an empty window is a
/// wrong answer that would be committed to the generated table.
fn cache_unreachable(msg: &str) -> color_eyre::Report {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://candle_cache@localhost:5432/candle_cache".to_string());
    eyre!(
        "candle-cache is unreachable ({msg}).\n\
         Tried DATABASE_URL={url}\n\
         Start the local PostgreSQL instance (or set DATABASE_URL) and re-run; \
         the generator will not compute a mask from an unreadable cache."
    )
}

/// Build an OANDA data source. `OANDA_TOKEN` is what the generator already
/// documents; `OANDA_API_KEY` is accepted as the alias the worker uses, so one
/// exported token serves both.
fn oanda_source() -> Result<oanda_client::data_source::OandaDataSource> {
    let token = std::env::var("OANDA_TOKEN")
        .or_else(|_| std::env::var("OANDA_API_KEY"))
        .map_err(|_| eyre!("OANDA_TOKEN (or OANDA_API_KEY) not set — required for OANDA fetch"))?;
    // The account id only scopes account endpoints; candle reads don't use it,
    // so an unset value is not worth failing a catalog-wide profiling run over.
    let account_id = std::env::var("OANDA_ACCOUNT_ID").unwrap_or_default();
    Ok(oanda_client::data_source::OandaDataSource::new(
        oanda_client::OandaClient::new(token),
        account_id,
    ))
}

/// Build a TradeNation client. `TN_ACCOUNT_TYPE=demo` (the default) needs no
/// credentials — spread profiles are market-wide, not account-specific.
fn tradenation_source() -> Result<tradenation_api::TradeNationClient> {
    let kind = std::env::var("TN_ACCOUNT_TYPE").unwrap_or_else(|_| "demo".to_string());
    match kind.to_ascii_lowercase().as_str() {
        "live" => {
            let user = std::env::var("TN_USERNAME")
                .map_err(|_| eyre!("TN_USERNAME not set (required for TN_ACCOUNT_TYPE=live)"))?;
            let pass = std::env::var("TN_PASSWORD")
                .map_err(|_| eyre!("TN_PASSWORD not set (required for TN_ACCOUNT_TYPE=live)"))?;
            Ok(tradenation_api::TradeNationClient::new(user, pass))
        }
        "demo" => Ok(tradenation_api::TradeNationClient::new_demo()),
        other => Err(eyre!(
            "TN_ACCOUNT_TYPE={other:?} not understood; use `demo` or `live`"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    /// Build a candle-cache bid/ask candle at `ts` whose CLOSE books are the
    /// given `mid`/`bid`/`ask`.
    ///
    /// The open/high/low books are deliberately set to **different** values
    /// rather than copies of the close. The profile is defined on closing
    /// spreads, and a helper that made all four equal cannot tell `c.close`
    /// from `c.open` — a mutation swapping them survived every test here until
    /// this helper discriminated the two books.
    fn cd(
        ts: DateTime<FixedOffset>,
        mid: f64,
        bid: f64,
        ask: f64,
    ) -> candle_model::BidAskCandleData {
        // A decoy open/high/low book: shifted mid AND a wider spread, so
        // reading the wrong book changes both of the asserted numbers.
        let (decoy_mid, decoy_bid, decoy_ask) = (mid + 0.5, bid + 0.48, ask + 0.52);
        candle_model::BidAskCandleData {
            timestamp: ts,
            open: decoy_mid,
            high: decoy_mid,
            low: decoy_mid,
            close: mid,
            bid_open: decoy_bid,
            bid_high: decoy_bid,
            bid_low: decoy_bid,
            bid_close: bid,
            ask_open: decoy_ask,
            ask_high: decoy_ask,
            ask_low: decoy_ask,
            ask_close: ask,
            volume: 0.0,
        }
    }

    /// The cache path must reduce a candle to exactly what the old adapter path
    /// produced: spread fraction off the CLOSE books, mid close carried, and the
    /// local hour stamped through the tz.
    #[test]
    fn cache_candle_reduces_to_the_same_minute_bar() {
        let ny = chrono_tz::America::New_York;
        // 22:00 UTC in July = 18:00 EDT.
        let utc = FixedOffset::east_opt(0).expect("utc offset");
        let ts = utc
            .with_ymd_and_hms(2026, 7, 15, 22, 0, 0)
            .single()
            .expect("unambiguous ts");

        let bars = minutes_from_cache(&[cd(ts, 1.0000, 0.9998, 1.0002)], ny);
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].local_hour, 18, "22:00 UTC in July is 18:00 EDT");
        assert_eq!(bars[0].utc_minute_of_day, 22 * 60);
        assert!((bars[0].spread_frac - 0.0004).abs() < 1e-12);
        assert_eq!(bars[0].mid_close, 1.0000);
    }

    /// A non-UTC-stamped candle must be converted, not read as wall-clock.
    /// candle-cache stamps `DateTime<FixedOffset>`, and a broker that answers
    /// in its own offset would otherwise land every bar in the wrong bucket —
    /// which silently moves the whole mask.
    #[test]
    fn cache_candle_timestamp_is_converted_not_truncated() {
        let ny = chrono_tz::America::New_York;
        // 08:00 +10:00 (Brisbane) is 22:00 UTC the previous day → 18:00 EDT.
        let bne = FixedOffset::east_opt(10 * 3600).expect("brisbane offset");
        let ts = bne
            .with_ymd_and_hms(2026, 7, 16, 8, 0, 0)
            .single()
            .expect("unambiguous ts");

        let bars = minutes_from_cache(&[cd(ts, 1.0000, 0.9998, 1.0002)], ny);
        assert_eq!(bars.len(), 1);
        assert_eq!(
            bars[0].utc_minute_of_day,
            22 * 60,
            "a +10:00 stamp must convert to 22:00 UTC, not be read as 08:00"
        );
        assert_eq!(bars[0].local_hour, 18);
    }

    /// Degenerate bars are dropped on the cache path exactly as on the old one.
    #[test]
    fn cache_candle_drops_degenerate_bars() {
        let ny = chrono_tz::America::New_York;
        let utc = FixedOffset::east_opt(0).expect("utc offset");
        let ts = utc
            .with_ymd_and_hms(2026, 7, 15, 22, 0, 0)
            .single()
            .expect("unambiguous ts");

        // Non-positive mid, and an inverted spread (ask < bid).
        let bars = minutes_from_cache(
            &[cd(ts, 0.0, 0.9998, 1.0002), cd(ts, 1.0, 1.0002, 0.9998)],
            ny,
        );
        assert!(bars.is_empty(), "both degenerate bars must be dropped");
    }

    /// The cache dir must stay per-broker: the table name derives from this
    /// path's final component, so one shared dir would collide two brokers'
    /// symbol spellings in a single table.
    #[test]
    fn cache_dir_is_per_broker() {
        let oanda = default_cache_dir(Broker::Oanda);
        let tn = default_cache_dir(Broker::TradeNation);
        assert_ne!(oanda, tn);
        assert_eq!(
            oanda.file_name().and_then(|s| s.to_str()),
            Some("candle_cache_oanda")
        );
        assert_eq!(
            tn.file_name().and_then(|s| s.to_str()),
            Some("candle_cache_tradenation")
        );
    }
}
