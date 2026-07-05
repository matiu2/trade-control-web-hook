//! Worker-agnostic dispatch results.
//!
//! The control-action handlers (`handle_status` / `handle_prep` / `handle_veto`
//! / … in the worker) historically returned a `worker::Response` directly,
//! which panics off-wasm and pins them to Cloudflare. [`ControlResult`] is the
//! worker-free carrier that replaces it: a `status` + `body` pair the wasm
//! worker maps to a `worker::Response` and the native runtime maps to an axum
//! response, at their respective edges. One handler, two edges, no drift.
//!
//! This is the control-action sibling of `ActionResult::Rejected`'s
//! `{ status, body }` shape (the broker-dispatch path). The entry/close/veto
//! path returns `ActionResult`; the prep/pause/register/plan path returns
//! `ControlResult`.

/// The outcome of a control-action handler: the HTTP status and body to return.
/// `2xx` statuses are successes (the body is the status YAML / `"ok"` / a plan
/// listing); everything else is a rejection or error (the body is the message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlResult {
    /// HTTP status to return (e.g. `200`, `400`, `409`, `500`).
    pub status: u16,
    /// Response body: the success payload for `2xx`, else the error message.
    pub body: String,
}

impl ControlResult {
    /// A `200 OK` with `body` (was `worker::Response::ok(body)`).
    pub fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
        }
    }

    /// A non-2xx error with `body` as the message (was
    /// `worker::Response::error(body, status)`).
    pub fn error(body: impl Into<String>, status: u16) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    /// True for a `2xx` status — lets an edge pick `Response::ok` vs
    /// `Response::error` to stay byte-faithful to the pre-refactor responses.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_is_200_success() {
        let r = ControlResult::ok("ok");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "ok");
        assert!(r.is_success());
    }

    #[test]
    fn error_carries_status_and_is_not_success() {
        let r = ControlResult::error("prep requires `step`", 400);
        assert_eq!(r.status, 400);
        assert!(!r.is_success());
    }
}
