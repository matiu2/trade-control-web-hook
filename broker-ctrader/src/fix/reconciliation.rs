//! A reconciliation may bypass only its own unresolved intent, retaining all locks.
use super::*;
use ctrader_fix::audit::Journal;

pub(crate) struct Lease {
    _gate: tokio::sync::OwnedMutexGuard<()>,
    _lock: evidence::AccountLock,
    journal: Journal,
    records: Vec<serde_json::Value>,
}
impl FixBroker {
    pub(crate) async fn pending_market_audit_ids(&self) -> Result<Vec<String>> {
        let _guard = self.gate.lock().await;
        let directory = evidence::reconciliation_directory(self)?;
        let entries = std::fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
        ensure!(
            entries.len() <= 10_000,
            "audit inventory exceeds reconciliation limit"
        );
        let mut pending = Vec::new();
        for entry in entries
            .iter()
            .filter(|e| e.path().extension().is_some_and(|s| s == "jsonl"))
        {
            let path = entry.path();
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| eyre!("invalid audit filename"))?;
            let (_journal, records) =
                Journal::resume(&path, &evidence::account_identity(self), id)?;
            let intent = &records[0]["evidence"]["request"];
            let resolved = records.iter().fold(false, |done, r| {
                if r["phase"] == "intent" {
                    false
                } else if r["phase"] == "resolved" && r["evidence"]["confirmed"] == true {
                    true
                } else {
                    done
                }
            });
            if !resolved
                && intent["operation"] == "open-api-entry"
                && intent["entry_kind"] == "market"
            {
                pending.push(id.to_owned());
            }
        }
        pending.sort();
        Ok(pending)
    }

    pub(crate) async fn reconcile(&self, audit_id: &str) -> Result<Lease> {
        let gate = self.gate.clone().lock_owned().await;
        let lock = evidence::AccountLock::reconciliation(self, audit_id)?;
        let path = lock.directory.join(format!("{audit_id}.jsonl"));
        let (journal, records) =
            Journal::resume(&path, &evidence::account_identity(self), audit_id)?;
        Ok(Lease {
            _gate: gate,
            _lock: lock,
            journal,
            records,
        })
    }
}
impl Lease {
    pub(crate) fn records(&self) -> &[serde_json::Value] {
        &self.records
    }
    pub(crate) fn record(&mut self, phase: &str, value: &serde_json::Value) -> Result<()> {
        self.journal.record(phase, value)
    }
}
