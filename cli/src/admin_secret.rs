//! Wrappers around `wrangler secret put / delete` for the credential
//! half of an account.
//!
//! The operator's wrangler credential never touches the worker request
//! path — it stays on the laptop where wrangler is configured. This
//! module just shells out so `account add` can do both halves in one
//! command instead of forcing the operator to remember the secret
//! binding name and pipe the JSON in by hand.
//!
//! `wrangler secret put <BINDING>` reads the secret value from stdin
//! when stdin is non-interactive (a pipe). We rely on that path so the
//! credential JSON never lands on disk.
//!
//! Both wrappers pass `--name <worker>` explicitly. Without it, wrangler
//! resolves the target Worker from a `wrangler.toml` in the *current*
//! directory — so running `trade-control account add` from anywhere but
//! the repo root fails with "Required Worker name missing". Passing the
//! name makes these commands cwd-independent.

use std::io::Write;
use std::process::{Command, Stdio};

use color_eyre::eyre::{Result, eyre};
use trade_control_core::account::Credentials;
use trade_control_core::intent::BrokerKind;

/// Compute the secret-binding name for an account. Must match the
/// worker-side [`super::accounts::secret_name_for`] *byte for byte* —
/// any drift here breaks the credential resolver in production.
///
/// Rules:
/// - `TN_ACCOUNT_<NAME>` for TradeNation
/// - `OANDA_ACCOUNT_<NAME>` for OANDA
/// - `<NAME>` is uppercased with `-` mapped to `_`
pub fn secret_binding_for(broker: BrokerKind, account_name: &str) -> String {
    let prefix = match broker {
        BrokerKind::TradeNation => "TN_ACCOUNT_",
        BrokerKind::Oanda => "OANDA_ACCOUNT_",
    };
    let normalised = account_name.to_ascii_uppercase().replace('-', "_");
    format!("{prefix}{normalised}")
}

/// Push the credential JSON to Cloudflare Secret Store via
/// `wrangler secret put <BINDING>`. Streams the JSON over stdin so
/// nothing transits through argv (where it would land in
/// `/proc/self/cmdline` and shell history).
///
/// The function returns once wrangler exits successfully. Wrangler
/// writes its own status to stderr — we don't capture it; the operator
/// sees the same "Uploaded secret X" line they'd see running wrangler
/// by hand.
pub fn put_secret(binding: &str, worker: &str, creds: &Credentials) -> Result<()> {
    let json = serde_json::to_string(creds).map_err(|e| eyre!("encode credentials: {e}"))?;
    let mut child = Command::new("wrangler")
        .arg("secret")
        .arg("put")
        .arg(binding)
        .arg("--name")
        .arg(worker)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| eyre!("spawn wrangler: {e} — is wrangler on PATH?"))?;
    // Take stdin first; if the child has already exited we'd hang trying
    // to write to a closed pipe otherwise.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| eyre!("wrangler stdin not available"))?;
    stdin
        .write_all(json.as_bytes())
        .map_err(|e| eyre!("write to wrangler stdin: {e}"))?;
    // Newline so wrangler treats the body as a complete line — some
    // wrangler versions block waiting for EOL before EOF.
    stdin
        .write_all(b"\n")
        .map_err(|e| eyre!("write newline to wrangler stdin: {e}"))?;
    drop(stdin);
    let status = child.wait().map_err(|e| eyre!("wait on wrangler: {e}"))?;
    if !status.success() {
        return Err(eyre!("wrangler secret put {binding} failed: {status}"));
    }
    Ok(())
}

/// Delete a credential secret. Mirror of [`put_secret`] for the
/// `account delete` cleanup path. Non-zero exit is surfaced — including
/// the "secret not found" case, which the caller can choose to ignore.
pub fn delete_secret(binding: &str, worker: &str) -> Result<()> {
    let status = Command::new("wrangler")
        .arg("secret")
        .arg("delete")
        .arg(binding)
        .arg("--name")
        .arg(worker)
        .status()
        .map_err(|e| eyre!("spawn wrangler: {e} — is wrangler on PATH?"))?;
    if !status.success() {
        return Err(eyre!("wrangler secret delete {binding} failed: {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tradenation_binding_uppercases_and_swaps_dashes() {
        assert_eq!(
            secret_binding_for(BrokerKind::TradeNation, "demo-alice"),
            "TN_ACCOUNT_DEMO_ALICE"
        );
    }

    #[test]
    fn oanda_binding_uses_correct_prefix() {
        assert_eq!(
            secret_binding_for(BrokerKind::Oanda, "live-prod"),
            "OANDA_ACCOUNT_LIVE_PROD"
        );
    }

    #[test]
    fn binding_leaves_alnum_intact() {
        assert_eq!(
            secret_binding_for(BrokerKind::TradeNation, "demo1"),
            "TN_ACCOUNT_DEMO1"
        );
    }
}
