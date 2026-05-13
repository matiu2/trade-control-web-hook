//! AEAD encryption for the encrypted half of the webhook payload.
//!
//! Format: `v1.BASE64(nonce || ciphertext || tag)` using ChaCha20-Poly1305.
//! AAD is a fixed protocol tag so different protocol versions can't be cross-replayed.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

/// 32-byte key length for ChaCha20-Poly1305.
pub const KEY_LEN: usize = 32;
/// 12-byte nonce length.
pub const NONCE_LEN: usize = 12;
/// AAD bound to this protocol version. Bumping this string is a hard break.
const AAD: &[u8] = b"trade-control-v1";
/// Prefix on the wire so we can rotate the algorithm later.
const PREFIX: &str = "v1.";

#[derive(Debug)]
pub enum CryptoError {
    BadKeyLen,
    BadPrefix,
    BadBase64,
    BadCiphertext,
    Aead,
}

impl core::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::BadKeyLen => "key must be 32 bytes",
            Self::BadPrefix => "payload missing v1. prefix",
            Self::BadBase64 => "payload is not valid base64",
            Self::BadCiphertext => "ciphertext too short (no nonce)",
            Self::Aead => "decryption failed",
        };
        f.write_str(s)
    }
}

impl std::error::Error for CryptoError {}

/// Decrypt and authenticate a `v1.<base64>` payload.
pub fn decrypt(key: &[u8], blob: &str) -> Result<Vec<u8>, CryptoError> {
    if key.len() != KEY_LEN {
        return Err(CryptoError::BadKeyLen);
    }
    let body = blob.strip_prefix(PREFIX).ok_or(CryptoError::BadPrefix)?;
    let raw = BASE64.decode(body).map_err(|_| CryptoError::BadBase64)?;
    if raw.len() < NONCE_LEN {
        return Err(CryptoError::BadCiphertext);
    }
    let (nonce_bytes, ct) = raw.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            Payload { msg: ct, aad: AAD },
        )
        .map_err(|_| CryptoError::Aead)
}

/// Encrypt + base64-encode a payload with a fresh random nonce.
///
/// The caller supplies the nonce so this module stays free of any RNG choice
/// (the worker target and the CLI use different sources). For tests we pass
/// fixed bytes; in production callers use a CSPRNG.
pub fn encrypt_with_nonce(
    key: &[u8],
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
) -> Result<String, CryptoError> {
    if key.len() != KEY_LEN {
        return Err(CryptoError::BadKeyLen);
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let ct = cipher
        .encrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad: AAD,
            },
        )
        .map_err(|_| CryptoError::Aead)?;
    let mut buf = Vec::with_capacity(NONCE_LEN + ct.len());
    buf.extend_from_slice(nonce);
    buf.extend_from_slice(&ct);
    Ok(format!("{PREFIX}{}", BASE64.encode(buf)))
}

/// Parse a 64-hex-char string into a 32-byte key.
pub fn parse_key_hex(s: &str) -> Result<[u8; KEY_LEN], CryptoError> {
    let trimmed = s.trim();
    let bytes = hex::decode(trimmed).map_err(|_| CryptoError::BadKeyLen)?;
    let arr: [u8; KEY_LEN] = bytes.try_into().map_err(|_| CryptoError::BadKeyLen)?;
    Ok(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; KEY_LEN] = [7u8; KEY_LEN];
    const NONCE: [u8; NONCE_LEN] = [3u8; NONCE_LEN];

    #[test]
    fn round_trip() {
        let plaintext = b"hello world, trade intent here";
        let blob = encrypt_with_nonce(&KEY, &NONCE, plaintext).unwrap();
        assert!(blob.starts_with("v1."));
        let recovered = decrypt(&KEY, &blob).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn wrong_key_rejected() {
        let blob = encrypt_with_nonce(&KEY, &NONCE, b"secret").unwrap();
        let wrong = [8u8; KEY_LEN];
        assert!(matches!(decrypt(&wrong, &blob), Err(CryptoError::Aead)));
    }

    #[test]
    fn tamper_rejected() {
        let blob = encrypt_with_nonce(&KEY, &NONCE, b"secret").unwrap();
        // flip a byte in the base64 body
        let mut chars: Vec<char> = blob.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        assert!(decrypt(&KEY, &tampered).is_err());
    }

    #[test]
    fn missing_prefix_rejected() {
        let blob = encrypt_with_nonce(&KEY, &NONCE, b"x").unwrap();
        let stripped = blob.strip_prefix("v1.").unwrap().to_string();
        assert!(matches!(
            decrypt(&KEY, &stripped),
            Err(CryptoError::BadPrefix)
        ));
    }

    #[test]
    fn malformed_base64_rejected() {
        assert!(matches!(
            decrypt(&KEY, "v1.!!!not-base64!!!"),
            Err(CryptoError::BadBase64)
        ));
    }

    #[test]
    fn short_ciphertext_rejected() {
        // "v1." + base64 of <12 bytes is too small to contain a nonce
        let blob = format!("v1.{}", BASE64.encode([0u8; 4]));
        assert!(matches!(
            decrypt(&KEY, &blob),
            Err(CryptoError::BadCiphertext)
        ));
    }

    #[test]
    fn bad_key_length() {
        let blob = encrypt_with_nonce(&KEY, &NONCE, b"x").unwrap();
        let short = [0u8; 16];
        assert!(matches!(
            decrypt(&short, &blob),
            Err(CryptoError::BadKeyLen)
        ));
    }

    #[test]
    fn parse_key_hex_round_trip() {
        let key = [0x42u8; KEY_LEN];
        let hex_str = hex::encode(key);
        assert_eq!(parse_key_hex(&hex_str).unwrap(), key);
    }

    #[test]
    fn parse_key_hex_rejects_wrong_length() {
        assert!(parse_key_hex("dead").is_err());
    }
}
