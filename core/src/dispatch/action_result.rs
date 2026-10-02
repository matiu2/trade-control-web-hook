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
    /// The broker call's outcome is **unknown**, not failed — the HTTP
    /// request to the broker succeeded but its response could not be
    /// read, so an order may or may not have actually been placed. This
    /// is recorded against the seen id exactly like [`Self::Ok`] (a
    /// retry could double an order that already filled — see
    /// [`crate::broker::EntryError::AmbiguousSuccess`]), but is rendered
    /// with its own distinct status/body so the caller is never told
    /// "entered" or "failed" when the truth is "go check the broker".
    AmbiguousSuccess(String),
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
            Self::AmbiguousSuccess(s) => format!("AmbiguousSuccess({s})"),
        }
    }

    /// The canonical, information-rich outcome string to persist in the
    /// request record — the *same* string `record_dispatcher_outcome` logs
    /// and the offline replay surfaces verbatim on its `BLOCKED — …` line.
    ///
    /// This deliberately differs from the HTTP response body for
    /// [`Self::Rejected`] and [`Self::Failed`]: the body is the caller-facing
    /// message (e.g. the long "entry blocked: SL too close to spread …"
    /// prose, or a flat `"action failed"`), whereas the recorded outcome is
    /// the concise gate verdict a later `plan timeline` / `status` read wants
    /// (e.g. `rejected: sl-widen-below-min-r (spread=1 widened_sl_lvl=… …)`).
    /// Persisting this instead of the body is what makes the live worker's
    /// record match what the replay already shows for the same reject.
    pub fn record_outcome(&self) -> &str {
        match self {
            Self::Ok(s) | Self::Failed(s) | Self::AmbiguousSuccess(s) => s,
            Self::Rejected { outcome, .. } => outcome,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_outcome_returns_the_rich_string_for_every_variant() {
        assert_eq!(
            ActionResult::Ok("entered".into()).record_outcome(),
            "entered"
        );
        assert_eq!(
            ActionResult::Failed("broker 500".into()).record_outcome(),
            "broker 500"
        );
        // For a rejection it is the concise `outcome`, NOT the human-facing
        // `body` — the two diverge on purpose (see the method doc).
        let reject = ActionResult::Rejected {
            status: 422,
            body: "entry blocked: SL too close to spread …".into(),
            outcome: "rejected: sl-widen-below-min-r (spread=1 …)".into(),
        };
        assert_eq!(
            reject.record_outcome(),
            "rejected: sl-widen-below-min-r (spread=1 …)"
        );
    }
}
