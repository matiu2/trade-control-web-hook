//! Calibration from the live account's own paired historical books.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod bake;
pub mod generate;

#[derive(Clone, Serialize, Deserialize)]
pub struct Report {
    pub account: String,
    pub account_id: String,
    pub source: String,
    pub cache_namespace: String,
    pub from_utc: DateTime<Utc>,
    pub to_utc: DateTime<Utc>,
    pub requested_symbols: Vec<String>,
    pub catalogue_symbols: Vec<String>,
    pub instruments: Vec<Measurement>,
    pub failures: Vec<(String, String)>,
    pub complete: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Measurement {
    pub symbol: String,
    pub schedule: String,
    pub pip_size: f64,
    pub first_utc: DateTime<Utc>,
    pub last_utc: DateTime<Utc>,
    #[serde(default)]
    pub observed_utc_days: usize,
    pub profile: crate::compute::SpreadProfile,
}
