//! Operator prose, kept separate from stable ledger outcomes and retry decisions.
use super::ActionResult;

mod classification;
mod steps;

impl ActionResult {
    /// Explain an unconfirmed entry without changing its recorded outcome.
    pub fn entry_warning(&self) -> String {
        if matches!(self, Self::Ok(_)) {
            return self.describe();
        }
        let (failure, recovery) = self
            .record_outcome()
            .split_once("; recovery: ")
            .unwrap_or((self.record_outcome(), ""));
        let category = match self {
            Self::Rejected { .. } => "LOCAL BLOCK",
            Self::AmbiguousSuccess(_) => "OUTCOME UNKNOWN",
            _ => classification::category(failure),
        };
        let mut message = format!("entry was not confirmed.\nFinal result: {category}");
        if !recovery.is_empty() {
            message.push_str(&format!(
                "\n\nSteps:\n{}",
                steps::render(recovery, category)
            ));
        }
        let reason = classification::reason(failure);
        if !recovery.contains(reason) || recovery.is_empty() {
            message.push_str(&format!("\n\nReason: {reason}"));
        }
        if let Self::Rejected { body, .. } = self
            && body != reason
        {
            message.push_str(&format!("\n{body}"));
        }
        message
    }
}

#[cfg(test)]
mod tests;
