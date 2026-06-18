//! Native-only helpers used by the `trade-control` CLI. Re-exports a small
//! surface so the binary doesn't need to poke at internal modules.

mod admin_client;
mod admin_secret;
mod calendar_bars;
mod control;
mod expiry;
mod forex_factory_cache;
mod history;
mod instruments;
mod interactive;
mod news_pattern;
mod pause_pattern;
mod prompts;
mod script_validator;
mod templates;
mod trade_patterns;

pub use admin_client::{
    AdoptBody, add_account, adopt_trade, delete_account, list_accounts, test_account,
};
pub use admin_secret::{delete_secret, put_secret, secret_binding_for};
pub use calendar_bars::{
    BuiltCalendarBundle, CalendarBarPlan, CalendarBarRow, CalendarBarsArgs, CalendarBrokerArg,
    PlanInputs, TimeframeArg, dedupe_and_filter_events, fetch_events_for_range, fetch_week_events,
    parse_instrument, plan_calendar_bars, plan_calendar_bars_within, print_summary_table,
    run_calendar_bars,
};
pub use control::{
    build_clear_prep_intent, build_clear_veto_intent, build_market_info_intent,
    build_plan_delete_intent, build_plan_list_intent, build_plan_show_intent, build_prep_intent,
    build_register_intent, build_status_intent, build_unlock_intent, build_veto_intent,
    wrap_signed, wrap_signed_direct_enter, wrap_signed_template,
};
/// Re-export forex-factory's event + impact types so downstream
/// consumers (tv-news, future strategy binaries) don't have to pin the
/// same git rev separately. They are part of the public API anyway —
/// `fetch_events_for_range` returns them.
pub use forex_factory::{EconomicEvent, Impact};
pub use history::{
    History, load as load_history, record_account_use, record_prep_use, record_veto_use,
};
pub use instruments::{load_cache, require_local_tn_account, validate_instrument};
pub use interactive::{fill_missing_fields, prompt_save_as_template};
pub use news_pattern::{
    BuiltNews, BuiltNewsAlert, NewsSpec, build_news_from_spec,
    load_spec_from_file as load_news_spec_from_file, write_news,
};
pub use pause_pattern::{
    BuiltPause, BuiltPauseAlert, PauseSpec, build_pause_from_spec,
    load_spec_from_file as load_pause_spec_from_file, write_pause,
};
pub use script_validator::{ScriptError, validate as validate_intent_scripts};
pub use templates::{discover_templates, pick_template_interactive, templates_root};
pub use trade_control_core::intent::{BrokerKind, PriceAnchor};
pub use trade_control_core::sig::KEY_LEN;
pub use trade_patterns::{
    BuiltAlert, BuiltTrade, EntryMode, MwSpec, PositionEnterSpec, PositionEntryKind, TradePattern,
    TradeSpec, build_position_enter, build_trade_from_spec, build_trade_interactive,
    load_spec_from_file, pick_pattern_interactive, write_trade,
};

/// Generate a fresh 32-byte signing key as 64 hex chars, using the OS RNG.
pub fn generate_key_hex() -> String {
    let mut bytes = [0u8; KEY_LEN];
    getrandom::fill(&mut bytes).expect("OS RNG");
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_key_hex_yields_64_chars() {
        assert_eq!(generate_key_hex().len(), 64);
    }
}
