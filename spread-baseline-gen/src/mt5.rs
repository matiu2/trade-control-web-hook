//! Account-pinned MT5 calibration reports, separate from the live baked table.
use candle_model::BidAskCandleData;
use chrono::{DateTime, Utc};
use color_eyre::{Result, eyre::ensure};
use serde::Serialize;

use crate::{SpreadProfile, cache::minutes_from_cache, profile_from_minutes};

#[derive(Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub feed: String,
    pub login: u64,
    pub server: String,
    pub from_utc: DateTime<Utc>,
    pub to_utc: DateTime<Utc>,
    pub requested_symbols: Vec<String>,
    pub complete: bool,
    pub instruments: Vec<InstrumentReport>,
    pub failures: Vec<Failure>,
}

#[derive(Serialize)]
pub struct Failure {
    pub symbol: String,
    pub error: String,
}

#[derive(Serialize)]
pub struct InstrumentReport {
    pub symbol: String,
    pub pip_size: f64,
    pub schedule: String,
    pub timezone: String,
    pub first_minute_utc: DateTime<Utc>,
    pub last_minute_utc: DateTime<Utc>,
    pub elevated_local_hours: Vec<u8>,
    pub profile: SpreadProfile,
}

pub fn instrument_report(
    symbol: &str,
    pip_size: f64,
    schedule: &str,
    timezone: chrono_tz::Tz,
    candles: &[BidAskCandleData],
) -> Result<InstrumentReport> {
    ensure!(
        pip_size.is_finite() && pip_size > 0.0,
        "invalid pip size for {symbol}"
    );
    ensure!(!candles.is_empty(), "no MT5 minute candles for {symbol}");
    ensure!(
        candles.windows(2).all(|c| c[0].timestamp < c[1].timestamp),
        "MT5 minute candles must be ascending and unique for {symbol}"
    );
    let bars = minutes_from_cache(candles, timezone);
    ensure!(
        bars.len() == candles.len(),
        "invalid MT5 closing quotes for {symbol}"
    );
    let profile = profile_from_minutes(&bars, pip_size);
    Ok(InstrumentReport {
        symbol: symbol.into(),
        pip_size,
        schedule: schedule.into(),
        timezone: timezone.to_string(),
        first_minute_utc: candles[0].timestamp.with_timezone(&Utc),
        last_minute_utc: candles[candles.len() - 1].timestamp.with_timezone(&Utc),
        elevated_local_hours: profile.elevated_vec(),
        profile,
    })
}

#[cfg(test)]
mod tests;
