//! cTrader platform adapter. Broker-neutral protocol lives in `ctrader-api`.
//! Strategy prices are reconstructed from paired historical quotes. Native BID
//! chart bars are exposed separately by [`market_data`] with their tick volume.
pub mod access;
mod candles;
pub mod catalogue;
mod entry;
mod history;
mod management;
pub mod market_data;
mod metadata;
pub mod reconciliation;
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
pub mod protection;
use open_api::OpenApiBroker;
mod facade;
mod fix;
mod hybrid;
pub use facade::CtraderBroker;
