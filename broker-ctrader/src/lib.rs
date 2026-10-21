//! cTrader platform adapter. Broker-neutral protocol lives in `ctrader-api`.
//! Prices are reconstructed from paired historical quotes, never bid-only bars.
pub mod access;
mod candles;
mod entry;
mod history;
mod management;
mod metadata;
mod session;
#[cfg(test)]
mod tests;
mod wire;

use chrono::{DateTime, Utc};
use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};
use ctrader_api::{Client, Config};
pub use metadata::InstrumentMetadata;
use serde_json::{Value, json};
use trade_control_core::broker::*;
use trade_control_core::settlement::Settlement;

mod open_api;
use open_api::OpenApiBroker;
mod facade;
mod fix;
mod hybrid;
pub use facade::CtraderBroker;
