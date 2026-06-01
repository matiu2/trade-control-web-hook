//! Classify one [`AlertResult`] from the JS template into a triaged
//! outcome, with a single source of truth for "did this alert actually
//! succeed?".
//!
//! TradingView's `create_alert` endpoint always replies HTTP 200 — the
//! real success/failure signal lives in the JSON body: `{"s":"ok",...}`
//! vs `{"s":"error","errmsg":"...","err":{"code":"..."}}`. The earlier
//! version of the pipeline trusted the HTTP status and logged every
//! 200 at INFO regardless of whether TV had rejected the alert, so
//! failures slipped past `RUST_LOG=info` runs unnoticed and the
//! `--create-alerts` exit code was always 0.
//!
//! This module exists so that classification (and its tests) is
//! independent of the logging glue around it.

use crate::create_alerts::AlertResult;

/// Triaged outcome for one POSTed alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// TradingView accepted the alert. Body parsed as `{"s":"ok",...}`.
    Ok,
    /// The Node process couldn't even attempt the POST — study not
    /// found, fetch threw, etc. The error string is the JS-side
    /// message.
    TransportError(String),
    /// TradingView replied with `{"s":"error",...}`. We surface the
    /// `errmsg` + nested `err.code` when present so the operator can
    /// distinguish `invalid_request` (malformed payload) from
    /// `general` (TV's catch-all) without re-reading the raw body.
    TvError {
        errmsg: Option<String>,
        err_code: Option<String>,
    },
    /// The JS returned neither status nor error — should not happen
    /// in practice. Treated as a failure for exit-code purposes.
    NoSignal,
}

impl Outcome {
    /// Whether the alert was successfully armed on TV. Used to drive
    /// the binary's exit code.
    pub fn is_success(&self) -> bool {
        matches!(self, Outcome::Ok)
    }
}

/// Classify a single [`AlertResult`]. Precedence:
///   1. JS-side transport error wins (no POST happened).
///   2. HTTP status present → parse the body for `{"s":"error",...}`.
///   3. Neither → [`Outcome::NoSignal`].
pub fn classify(result: &AlertResult) -> Outcome {
    if let Some(err) = &result.error {
        return Outcome::TransportError(err.clone());
    }
    if result.status.is_none() {
        return Outcome::NoSignal;
    }
    let body = result.body.as_deref().unwrap_or("");
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Outcome::Ok,
    };
    let s = parsed.get("s").and_then(|v| v.as_str());
    if s == Some("error") {
        let errmsg = parsed
            .get("errmsg")
            .and_then(|v| v.as_str())
            .map(String::from);
        let err_code = parsed
            .get("err")
            .and_then(|e| e.get("code"))
            .and_then(|v| v.as_str())
            .map(String::from);
        return Outcome::TvError { errmsg, err_code };
    }
    Outcome::Ok
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn r(status: Option<i64>, body: Option<&str>, error: Option<&str>) -> AlertResult {
        AlertResult {
            name: Some("05-enter.yaml".into()),
            status,
            body: body.map(String::from),
            error: error.map(String::from),
            debug: Some(json!({"tool": null, "drawing_id": null})),
        }
    }

    #[test]
    fn status_200_with_ok_body_is_success() {
        let out = classify(&r(Some(200), Some(r#"{"s":"ok","id":"x"}"#), None));
        assert_eq!(out, Outcome::Ok);
        assert!(out.is_success());
    }

    #[test]
    fn status_200_with_error_body_is_tv_error() {
        let body = r#"{"s":"error","id":"maav-150267828","r":null,"errmsg":"error","err":{"code":"general"}}"#;
        let out = classify(&r(Some(200), Some(body), None));
        assert_eq!(
            out,
            Outcome::TvError {
                errmsg: Some("error".into()),
                err_code: Some("general".into()),
            }
        );
        assert!(!out.is_success());
    }

    #[test]
    fn invalid_request_extracts_code() {
        let body = r#"{"s":"error","id":"x","errmsg":"bad","err":{"code":"invalid_request"}}"#;
        match classify(&r(Some(200), Some(body), None)) {
            Outcome::TvError { err_code, .. } => {
                assert_eq!(err_code.as_deref(), Some("invalid_request"))
            }
            other => panic!("expected TvError, got {other:?}"),
        }
    }

    #[test]
    fn transport_error_short_circuits() {
        let out = classify(&r(None, None, Some("study not found")));
        assert_eq!(out, Outcome::TransportError("study not found".into()));
        assert!(!out.is_success());
    }

    #[test]
    fn transport_error_wins_over_status() {
        // Defensive: if the JS ever emits both, the transport error
        // is the more actionable signal.
        let out = classify(&r(Some(200), Some(r#"{"s":"ok"}"#), Some("explode")));
        assert!(matches!(out, Outcome::TransportError(_)));
    }

    #[test]
    fn missing_signal_is_no_signal() {
        let out = classify(&r(None, None, None));
        assert_eq!(out, Outcome::NoSignal);
        assert!(!out.is_success());
    }

    #[test]
    fn unparseable_body_treated_as_success() {
        // The JS truncates the body at 2 KB — if the truncation lands
        // mid-JSON, we shouldn't flip a successful POST to a failure
        // just because we can't parse it. HTTP 200 + un-parseable is
        // best-treated as success.
        let out = classify(&r(Some(200), Some("not json at all"), None));
        assert_eq!(out, Outcome::Ok);
    }
}
