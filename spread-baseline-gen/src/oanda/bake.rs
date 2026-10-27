//! Only a complete account-pinned 90-day measurement may become runtime defaults.

use super::Report;
use crate::{BaselineRow, Broker, compute::ReviewStatus};
use color_eyre::{Result, eyre::ensure};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub fn render(report: &Report) -> Result<String> {
    ensure!(
        !report.account_id.is_empty()
            && report
                .account_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
            && report.cache_namespace == oanda_client::account_candle_namespace(&report.account_id),
        "Invalid OANDA account cache provenance"
    );
    ensure!(
        report.complete && report.failures.is_empty(),
        "Live OANDA calibration is incomplete"
    );
    ensure!(
        report.source == "oanda-live-account-candles-MBA",
        "Wrong OANDA candle pricing source"
    );
    ensure!(
        !report.account.is_empty()
            && report
                .account
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        "Invalid account name"
    );
    ensure!(
        report.to_utc <= chrono::Utc::now()
            && report.to_utc - report.from_utc >= chrono::Duration::days(90),
        "Live spread bake requires 90 closed calendar days"
    );
    let wanted = report.requested_symbols.iter().collect::<BTreeSet<_>>();
    ensure!(
        wanted == report.catalogue_symbols.iter().collect()
            && wanted.len() == report.catalogue_symbols.len(),
        "Live bake must cover the complete broker account catalogue"
    );
    let measured = report
        .instruments
        .iter()
        .map(|row| &row.symbol)
        .collect::<BTreeSet<_>>();
    ensure!(
        !wanted.is_empty()
            && wanted.len() == report.requested_symbols.len()
            && measured.len() == report.instruments.len()
            && wanted == measured,
        "Live account catalogue is incomplete or duplicated"
    );
    let rows = report
        .instruments
        .iter()
        .map(|row| {
            validate_measurement(row, report.from_utc, report.to_utc)?;
            Ok(BaselineRow {
                broker: Broker::Oanda,
                symbol: format!("oanda:{}:{}", report.account, row.symbol),
                display_name: row.symbol.clone(),
                spread_schedule: row.schedule.clone(),
                profile: row.profile.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let table =
        crate::render_table(&rows).replace("SPREAD_BASELINE_CANDLE", "SPREAD_BASELINE_OANDA");
    let fingerprint = format!("{:x}", Sha256::digest(report.account_id.as_bytes()));
    Ok(format!(
        "// Account={}, account identity SHA256={fingerprint}; source={}.\n// Measured {} through {}.\n{table}",
        report.account, report.source, report.from_utc, report.to_utc
    ))
}

/// Reject a short/truncated successful response as well as unusable profiles.
/// A trading week at each edge allows weekends/holidays; at least 80% of the
/// normal weekday count must be represented across the requested window.
pub(super) fn validate_measurement(
    row: &super::Measurement,
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    validate_profile(row)?;
    ensure!(
        matches!(
            row.profile.review,
            ReviewStatus::Reviewed | ReviewStatus::MaskNotComputable
        ) && row.profile.baseline_median_pips > 0.0,
        "Unreviewed live profile: {}",
        row.symbol
    );
    ensure!(
        row.first_utc >= from && row.last_utc < to && row.first_utc <= row.last_utc,
        "Invalid candle bounds for {}",
        row.symbol
    );
    let week = chrono::Duration::days(7);
    ensure!(
        row.first_utc <= from + week && row.last_utc >= to - week,
        "Live history does not span the calibration window: {}",
        row.symbol
    );
    let minimum_days = ((to - from).num_days() * 4 + 6) / 7;
    ensure!(
        row.observed_utc_days >= usize::try_from(minimum_days)?,
        "Too few observed trading dates for {}: {} (minimum {minimum_days})",
        row.symbol,
        row.observed_utc_days
    );
    Ok(())
}

fn validate_profile(row: &super::Measurement) -> Result<()> {
    let profile = &row.profile;
    ensure!(
        row.pip_size.is_finite()
            && row.pip_size > 0.0
            && profile.n_bars >= crate::compute::MIN_INSTRUMENT_BARS,
        "Insufficient paired live candles for {}",
        row.symbol
    );
    ensure!(
        [
            profile.baseline_low_pips,
            profile.baseline_median_pips,
            profile.baseline_high_pips
        ]
        .iter()
        .all(|value| value.is_finite() && *value >= 0.0)
            && profile.baseline_low_pips <= profile.baseline_median_pips
            && profile.baseline_median_pips <= profile.baseline_high_pips,
        "Invalid spread percentiles for {}",
        row.symbol
    );
    ensure!(
        profile
            .hour_widen_frac
            .iter()
            .chain(&profile.hour_p90_frac)
            .all(|value| value.is_finite() && *value >= 0.0),
        "Invalid hourly spreads for {}",
        row.symbol
    );
    ensure!(
        profile.elevated_hours >> 24 == 0,
        "Invalid spread-hour mask for {}",
        row.symbol
    );
    Ok(())
}

#[cfg(test)]
mod tests;
