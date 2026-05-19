//! Pure helpers used by `tn_login`, factored out so they can be
//! tested on the host target. The `tn_login` module itself is
//! wasm-only (it links `worker::Fetch`, which has no native shim),
//! so its inline tests don't run during `cargo test`.

// String errors keep this module dependency-free; the wasm caller in
// `tn_login.rs` wraps them into `worker::Error::RustError`.

/// Pick a trading account id out of the auth0/user JSON payload.
///
/// Preference order, matching the native `tradenation_api` flow:
///   1. First **active** account with `balance.cash_balance > 0`.
///   2. Else, first active account regardless of balance.
///   3. Else, error — there's no point trying cloudtrade/login.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn pick_funded_account(json: &str) -> Result<u64, String> {
    let parsed: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("tn live login: parse auth0/user json: {e}"))?;
    let accounts = parsed
        .get("app_metadata")
        .and_then(|m| m.get("trading_accounts"))
        .and_then(|a| a.as_array())
        .ok_or_else(|| {
            "tn live login: auth0/user missing app_metadata.trading_accounts".to_owned()
        })?;
    let active: Vec<&serde_json::Value> = accounts
        .iter()
        .filter(|a| a.get("status").and_then(|s| s.as_str()) == Some("active"))
        .collect();
    let funded = active.iter().copied().find(|a| {
        a.get("balance")
            .and_then(|b| b.get("cash_balance"))
            .and_then(|c| c.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .is_some_and(|b| b > 0.0)
    });
    let chosen = funded
        .or_else(|| active.first().copied())
        .ok_or_else(|| "tn live login: no active trading accounts found".to_owned())?;
    chosen
        .get("id")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "tn live login: chosen account missing id".to_owned())
}

/// Clip a response body for logging. TN error pages can be kilobytes;
/// we only want enough context to tell the operator what broke.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn truncate_for_log(s: &str) -> String {
    const MAX: usize = 200;
    if s.len() <= MAX {
        s.to_owned()
    } else {
        format!("{}…", &s[..MAX])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_funded_account_prefers_funded_active() {
        // Two active accounts; only the second is funded. Picker
        // should pick the funded one even though it's not first.
        let json = r#"{
            "app_metadata": {
                "trading_accounts": [
                    {"id": 1, "status": "active", "balance": {"cash_balance": "0.00"}},
                    {"id": 2, "status": "active", "balance": {"cash_balance": "991.67"}},
                    {"id": 3, "status": "inactive", "balance": {"cash_balance": "500.00"}}
                ]
            }
        }"#;
        assert_eq!(pick_funded_account(json).unwrap(), 2);
    }

    #[test]
    fn pick_funded_account_falls_back_to_first_active() {
        // No funded active accounts — fall back to the first active.
        let json = r#"{
            "app_metadata": {
                "trading_accounts": [
                    {"id": 7, "status": "inactive", "balance": {"cash_balance": "1000.00"}},
                    {"id": 8, "status": "active",   "balance": {"cash_balance": "0.00"}},
                    {"id": 9, "status": "active",   "balance": {"cash_balance": "0.00"}}
                ]
            }
        }"#;
        assert_eq!(pick_funded_account(json).unwrap(), 8);
    }

    #[test]
    fn pick_funded_account_errors_when_no_active() {
        let json = r#"{
            "app_metadata": {
                "trading_accounts": [
                    {"id": 1, "status": "inactive"}
                ]
            }
        }"#;
        assert!(pick_funded_account(json).is_err());
    }

    #[test]
    fn pick_funded_account_errors_on_missing_path() {
        // app_metadata.trading_accounts missing entirely.
        let json = r#"{"sub": "abc"}"#;
        assert!(pick_funded_account(json).is_err());
    }

    #[test]
    fn pick_funded_account_handles_missing_balance() {
        // Active account with no balance field at all — should still
        // be selected as the fallback first-active.
        let json = r#"{
            "app_metadata": {
                "trading_accounts": [
                    {"id": 42, "status": "active"}
                ]
            }
        }"#;
        assert_eq!(pick_funded_account(json).unwrap(), 42);
    }

    #[test]
    fn truncate_for_log_short_passes_through() {
        assert_eq!(truncate_for_log("hello"), "hello");
    }

    #[test]
    fn truncate_for_log_long_is_clipped() {
        let s = "x".repeat(500);
        let out = truncate_for_log(&s);
        assert!(out.ends_with('…'));
        // 200 payload chars + the ellipsis.
        assert_eq!(out.chars().filter(|c| *c == 'x').count(), 200);
    }
}
