//! Native-only helpers used by the `trade-control` CLI. Re-exports a small
//! surface so the binary doesn't need to poke at internal modules.

mod admin_client;
mod admin_secret;
mod control;
mod expiry;
mod history;
mod interactive;
mod prompts;
mod templates;
mod trade_patterns;

pub use admin_client::{add_account, delete_account, list_accounts, test_account};
pub use admin_secret::{delete_secret, put_secret, secret_binding_for};
pub use control::{
    build_clear_prep_intent, build_clear_veto_intent, build_prep_intent, build_status_intent,
    build_unlock_intent, build_veto_intent, wrap_signed, wrap_signed_template,
};
pub use history::{History, record_account_use, record_prep_use, record_veto_use};
pub use interactive::{fill_missing_fields, prompt_save_as_template};
pub use templates::{discover_templates, pick_template_interactive, templates_root};
pub use trade_control_core::sig::KEY_LEN;
pub use trade_patterns::{
    BuiltAlert, BuiltTrade, TradePattern, TradeSpec, build_trade_from_spec,
    build_trade_interactive, load_spec_from_file, pick_pattern_interactive, write_trade,
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
