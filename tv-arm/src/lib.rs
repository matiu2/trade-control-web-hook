//! Shared setup arming for the CLI and chart applications.
//!
//! Use [`api::run_frozen`] from a blocking thread: broker and calendar adapters
//! bridge synchronous orchestration to their own async runtimes.
#![allow(dead_code)]

pub mod account_reads;
pub mod api;
pub mod args;
mod broker_kind;
mod broker_read;
mod calendar;
mod clipboard;
mod control_bundle;
mod control_windows;
mod ctrader_instrument;
pub mod frozen_position;
pub mod frozen_setup;
mod geometry;
mod hs_resolve;
mod instrument_recovery;
mod instrument_resolution;
mod ma_cross_close;
mod ma_cross_veto;
mod ma_requirements;
mod mt5_instrument;
mod mw_geometry;
mod mw_resolve;
mod news_marker;
mod news_window;
pub mod option_schema;
mod pattern_banner;
mod pipeline;
pub mod plan_geometry;
mod position_entry;
mod position_trade;
mod precision;
mod register;
mod register_post;
mod replay;
mod resolve_error;
mod roles;
mod save_fixture;
mod save_matrix;
mod sentiment;
mod setup_inputs;
pub mod sl_anchor;
mod sl_note;
mod spec_url;
mod spread;
mod spread_retry;
mod start_note;
#[cfg(test)]
mod test_drawings;
mod timeframe;
mod trade_plan_build;

/// Run the existing command-line orchestration with its baked environment.
pub fn run(args: args::Args) -> color_eyre::Result<i32> {
    pipeline::run(args)
}
