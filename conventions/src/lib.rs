//! Shared constants and vocabularies for the trade-control stack.
//!
//! This crate is the single source of truth for:
//! - Alert basenames (`01-veto-too-high`, `05-enter`, etc.) — see
//!   [`AlertBasename`].
//! - Drawing-label vocabularies the chart-reader accepts — see
//!   [`labels`].
//! - Pine plot IDs that drive `05-enter` / `06-close-on-reversal`
//!   alerts — see [`pine`].
//! - Broker enum and exchange/symbol mappings — see [`broker`] and
//!   [`instrument`].
//!
//! Crate is intentionally dependency-light: any future strategy
//! binary (M/W reversal, etc.) and both the Cloudflare worker and the
//! `trade-control` CLI can depend on it without dragging in builder
//! code.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod basenames;
mod broker;
mod instrument;
mod labels;
mod pine;

pub use basenames::AlertBasename;
pub use broker::Broker;
pub use instrument::{instrument_for, split_symbol};
pub use labels::{
    BLACKOUT_END_LABELS, BLACKOUT_START_LABELS, BREAK_LABELS, INVALIDATION_LABELS, MW_PATH_LABELS,
    NEWS_END_LABELS, NEWS_START_LABELS, PREP_BREAK_AND_CLOSE, PREP_EXPIRY_SUFFIX, PREP_RETEST,
    RETEST_LABELS, SR_LEVEL_LABELS, TRADE_EXPIRY_LABELS, matches, prep_name_from_expiry_label,
};
pub use pine::{
    Direction, PINE_INDICATOR_NAME, PLOT_EVERY_BAR_CLOSE, PLOT_LONG_PATTERN, PLOT_SHORT_PATTERN,
    entry_plot_for, mw_direction_from_label, reversal_close_plot_for,
};
