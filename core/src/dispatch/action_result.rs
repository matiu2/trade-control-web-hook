//! The broker-dispatch outcome carrier.

/// Outcome of a broker-dispatched action (`Enter` / `Close` / `Invalidate` /
/// escalated `Veto`). The consumption edge (the wasm worker's HTTP dispatcher
/// or the native runtime) maps it to a response and decides whether to record
/// the intent id as seen.
///
/// Only [`ActionResult::Ok`] lands in the seen-by-id index — see the worker's
/// `record_dispatcher_outcome` for why. `Failed` and `Rejected` outcomes are
/// logged via `tracing` for post-mortem visibility but do not consume the
/// intent id.
pub enum ActionResult {
    /// Action completed successfully. The outcome (e.g. `"entered"`)
    /// is recorded against the seen id so a replay of the same alert
    /// body 409s instead of placing a duplicate order.
    Ok(String),
    /// Action reached the broker but the broker call failed. HTTP
    /// response is 502. **Not** recorded against the seen id — the
    /// next fire is allowed to retry.
    Failed(String),
    /// Action was rejected before reaching the broker (gate, validation,
    /// state error). The `status` + `body` are returned to the caller as
    /// the HTTP rejection (built at the consumption edge). **Not**
    /// recorded against the seen id — gate rejections are transient
    /// (the condition might flip later in the alert window), so the
    /// next fire is allowed through.
    Rejected {
        /// HTTP status for the rejection (e.g. 412, 500, 502).
        status: u16,
        /// Response body text (was the `Response::error` message).
        body: String,
        outcome: String,
    },
}

impl ActionResult {
    /// Short, `Response`-free description for logging. Used by the
    /// spread-blackout restore re-drive, which logs the outcome but does not
    /// route it through the HTTP dispatcher.
    pub fn describe(&self) -> String {
        match self {
            Self::Ok(s) => format!("Ok({s})"),
            Self::Failed(s) => format!("Failed({s})"),
            Self::Rejected { outcome, .. } => format!("Rejected({outcome})"),
        }
    }
}
