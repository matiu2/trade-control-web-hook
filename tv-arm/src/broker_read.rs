//! Live broker reads performed at arm time.
//!
//! Both are deliberately **blocking** and both **hard-error** rather than
//! falling back to a guess, because a guessed value here is silently wrong in a
//! way nothing downstream can detect:
//!
//! - the **spread** is baked onto the M/W enter so the worker can mid→bid/ask
//!   correct entry/SL/TP at fill time. A stale or defaulted spread mis-sizes
//!   every one of those.
//! - the **mid** is the `--pull-back` anchor — it *is* "price at arm time" by
//!   definition, so there is no sensible default at all.
//!
//! There is no operator override for either. See `frozen_setup` for why these
//! are re-read on every arm rather than frozen into a spec.

use color_eyre::eyre::{Context, Result};
use trade_control_conventions::Broker;

use crate::args::Args;
use crate::resolve_error::ResolveError;

/// Read the live broker spread (in pips) on the shared CLI runtime so pooled
/// MT5 sockets remain usable by subsequent broker reads.
/// Any read failure (no token, network error, market closed,
/// degenerate spread) surfaces as a `Fatal` resolve error carrying the
/// actionable message from `spread::read_spread_pips`.
pub fn read_spread_blocking(
    args: &Args,
    broker: Broker,
    instrument: &str,
    pip_size: f64,
    account: &str,
) -> std::result::Result<f64, ResolveError> {
    if matches!(broker, Broker::Oanda | Broker::TradeNation) {
        return read_worker_spread(args, instrument, pip_size, account)
            .map_err(ResolveError::Fatal);
    }
    trade_control_cli::blocking::run_blocking(crate::spread::read_spread_pips(
        broker,
        instrument,
        pip_size,
        Some(account),
    ))
    .map_err(ResolveError::Fatal)
}

/// Blocking live **mid** read — the pullback prep's arm-time anchor. Same
/// runtime-bridge shape as [`read_spread_blocking`]; hard-errors on a
/// stale/degenerate quote so a bad anchor can't silently mis-fire the pullback.
pub fn read_mid_blocking(
    broker: Broker,
    instrument: &str,
    account: &str,
    key: &[u8; trade_control_core::sig::KEY_LEN],
    execution: Option<&crate::api::Execution>,
) -> Result<f64> {
    if matches!(broker, Broker::Oanda | Broker::TradeNation) {
        let client = crate::account_reads::AccountReads::new(
            crate::register_post::worker_url(execution),
            key,
        )?;
        let (bid, ask) =
            trade_control_cli::blocking::run_blocking(client.quote(account, instrument))?;
        crate::spread::spread_from_bid_ask(bid, ask)?;
        return Ok((bid + ask) / 2.0);
    }
    trade_control_cli::blocking::run_blocking(crate::spread::read_mid(
        broker,
        instrument,
        Some(account),
    ))
    .context("reading live broker mid")
}

fn read_worker_spread(args: &Args, instrument: &str, pip_size: f64, account: &str) -> Result<f64> {
    if !pip_size.is_finite() || pip_size <= 0.0 {
        return Err(color_eyre::eyre::eyre!(
            "pip_size must be finite and positive; got {pip_size}"
        ));
    }
    let (bid, ask) = read_worker_quote(args, instrument, account)?;
    Ok(crate::spread::spread_from_bid_ask(bid, ask)? / pip_size)
}

fn read_worker_quote(args: &Args, instrument: &str, account: &str) -> Result<(f64, f64)> {
    let key = crate::pipeline::read_key(args)?;
    let client = crate::account_reads::AccountReads::new(
        crate::register_post::worker_url(args.execution.as_ref()),
        &key,
    )?;
    trade_control_cli::blocking::run_blocking(client.quote(account, instrument))
        .context("Read selected account quote from worker")
}

#[cfg(test)]
mod tests;
