//! Native-only helpers used by the `encrypt-payload` CLI. Re-exports a small
//! surface so the binary doesn't need to poke at internal modules.

use color_eyre::eyre::{Result, eyre};

mod interactive;
mod prompts;

pub use crate::crypto::{KEY_LEN, NONCE_LEN};
pub use interactive::fill_missing_fields;

/// Generate a fresh 32-byte key as 64 hex chars, using the OS RNG.
pub fn generate_key_hex() -> String {
    let mut bytes = [0u8; KEY_LEN];
    getrandom::fill(&mut bytes).expect("OS RNG");
    hex::encode(bytes)
}

/// Encrypt a JSON intent under `key`, returning the `v1.<base64>` blob.
pub fn encrypt_intent(key: &[u8; KEY_LEN], plaintext: &[u8]) -> Result<String> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| eyre!("getrandom: {e}"))?;
    crate::crypto::encrypt_with_nonce(key, &nonce, plaintext).map_err(|e| eyre!("encrypt: {e}"))
}

/// Build the YAML body the user pastes into the TradingView alert template.
/// Plaintext fields use TradingView placeholders; the encrypted blob is fixed.
pub fn build_yaml_template(blob: &str) -> String {
    format!(
        "close: {{{{close}}}}\n\
         high: {{{{high}}}}\n\
         low: {{{{low}}}}\n\
         time: \"{{{{time}}}}\"\n\
         payload: \"{blob}\"\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto;

    #[test]
    fn cli_encrypt_decrypts_via_crypto_module() {
        // The CLI uses the OS RNG for the nonce; the worker decrypts via crypto::decrypt.
        // Round-tripping here proves the two halves agree on the format.
        let key_hex = generate_key_hex();
        let key = crypto::parse_key_hex(&key_hex).unwrap();
        let payload = b"{\"v\":1,\"id\":\"x\"}";
        let blob = encrypt_intent(&key, payload).unwrap();
        let decrypted = crypto::decrypt(&key, &blob).unwrap();
        assert_eq!(decrypted, payload);
    }

    #[test]
    fn generate_key_hex_yields_64_chars() {
        assert_eq!(generate_key_hex().len(), 64);
    }

    #[test]
    fn build_yaml_template_contains_placeholders() {
        let yaml = build_yaml_template("v1.deadbeef");
        assert!(yaml.contains("close: {{close}}"));
        assert!(yaml.contains("payload: \"v1.deadbeef\""));
    }
}
