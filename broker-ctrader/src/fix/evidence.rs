//! A synced intent is required before a management write; unresolved intents block later writes.
use super::*;
use ctrader_fix::{audit::Journal, trading::MutationOutcome};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
const MAX_FILES: usize = 10_000;
const MAX_FILE: u64 = 16 * 1024 * 1024;

pub(super) struct AccountLock {
    _file: File,
    pub directory: PathBuf,
}
pub(super) struct Mutation {
    journal: Journal,
    identity: Value,
    client_order_id: String,
}
fn account_key(broker: &FixBroker) -> String {
    let hash = broker
        .trade
        .sender_comp_id
        .bytes()
        .chain([0])
        .chain(broker.trade.target_comp_id.bytes())
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    format!("{hash:016x}")
}
fn state_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CTRADER_FIX_AUDIT_DIR") {
        ensure!(!path.is_empty(), "CTRADER_FIX_AUDIT_DIR must not be empty");
        return Ok(path.into());
    }
    let root = std::env::var_os("XDG_STATE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| eyre!("cTrader FIX audit directory unavailable; no mutation sent"))?;
    Ok(root.join("trade-control/ctrader-fix"))
}
pub(super) fn reconciliation_directory(broker: &FixBroker) -> Result<PathBuf> {
    let root = state_root()?;
    private_directory(&root)?;
    let directory = root.join(account_key(broker));
    private_directory(&directory)?;
    Ok(directory)
}
fn private_directory(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    ensure!(
        !absolute
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir)),
        "cTrader FIX audit paths cannot contain parent traversal"
    );
    let mut chain: Vec<_> = absolute.ancestors().map(Path::to_path_buf).collect();
    chain.reverse();
    for directory in &chain {
        match fs::symlink_metadata(directory) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "cTrader FIX audit directory ancestry contains a non-directory or symlink"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new().mode(0o700).create(directory)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = fs::symlink_metadata(&absolute)?;
    // SAFETY: geteuid has no arguments or preconditions.
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == 0o700,
        "cTrader FIX audit directory must be an owned 0700 directory; no mutation sent"
    );
    // Sync the whole ancestry even on retry: an earlier failed setup may have left
    // existing directories whose parent links were never persisted.
    persist_directory_links(&chain, |directory| {
        File::open(directory)?.sync_all().map_err(Into::into)
    })
}
fn persist_directory_links(
    chain: &[PathBuf],
    mut sync: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    for directory in chain.iter().rev() {
        sync(directory)?;
    }
    Ok(())
}

impl AccountLock {
    pub fn acquire(broker: &FixBroker) -> Result<Self> {
        Self::acquire_at(broker, &state_root()?)
    }
    pub(super) fn acquire_at(broker: &FixBroker, root: &Path) -> Result<Self> {
        Self::acquire_inner(broker, root, None)
    }
    pub(super) fn reconciliation(broker: &FixBroker, audit_id: &str) -> Result<Self> {
        ensure!(
            !audit_id.is_empty()
                && audit_id.len() <= 50
                && audit_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c)),
            "invalid reconciliation audit ID"
        );
        Self::acquire_inner(broker, &state_root()?, Some(audit_id))
    }
    fn acquire_inner(broker: &FixBroker, root: &Path, allowed: Option<&str>) -> Result<Self> {
        private_directory(root)?;
        let directory = root.join(account_key(broker));
        private_directory(&directory)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(directory.join("account.lock"))?;
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no arguments or preconditions.
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600,
            "cTrader FIX account lock must be an owned 0600 regular file"
        );
        // SAFETY: flock receives an owned open file descriptor and no pointers.
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another cTrader FIX management operation is active; no mutation sent"
        );
        check_resolved_except(&directory, allowed)?;
        Ok(Self {
            _file: file,
            directory,
        })
    }
}
fn check_resolved(directory: &Path) -> Result<()> {
    check_resolved_except(directory, None)
}
fn check_resolved_except(directory: &Path, allowed: Option<&str>) -> Result<()> {
    let entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        entries.len() <= MAX_FILES,
        "cTrader FIX audit directory exceeds file limit; no mutation sent"
    );
    for entry in entries
        .into_iter()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
    {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(entry.path())?;
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no arguments or preconditions.
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600
                && metadata.len() <= MAX_FILE,
            "invalid cTrader FIX audit file; no mutation sent"
        );
        let mut contents = String::new();
        file.take(MAX_FILE + 1).read_to_string(&mut contents)?;
        ensure!(
            contents.len() as u64 <= MAX_FILE && contents.ends_with('\n'),
            "incomplete cTrader FIX audit evidence; reconcile before further mutations"
        );
        let mut resolved = false;
        for line in contents.lines() {
            let record: Value = serde_json::from_str(line)
                .map_err(|_| eyre!("invalid cTrader FIX audit evidence; no mutation sent"))?;
            if record["phase"] == "intent" {
                resolved = false;
            }
            if record["phase"] == "resolved" && record["evidence"]["confirmed"] == true {
                resolved = true;
            }
        }
        ensure!(
            resolved
                || allowed.is_some_and(|id| entry.file_name() == format!("{id}.jsonl").as_str()),
            "unresolved cTrader FIX mutation intent exists; reconcile its recorded client order ID before further mutations"
        );
    }
    Ok(())
}
impl Mutation {
    pub fn id() -> String {
        format!(
            "manage-{}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_micros(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        )
    }
    pub fn prepare(
        broker: &FixBroker,
        lock: &AccountLock,
        id: String,
        mut intent: Value,
    ) -> Result<Self> {
        check_resolved(&lock.directory)?;
        intent["client_order_id"] = json!(id);
        let identity = account_identity(broker);
        let journal = append_with_receipt(&receipt(&identity, &id, "intent", &intent), || {
            Journal::prepare(
                &lock.directory.join(format!("{id}.jsonl")),
                &identity,
                &intent,
            )
        })?;
        Ok(Self {
            journal,
            identity,
            client_order_id: id,
        })
    }
    pub fn client_order_id(&self) -> &str {
        &self.client_order_id
    }
    pub fn record(&mut self, phase: &str, evidence: &Value) -> Result<()> {
        let receipt = receipt(&self.identity, &self.client_order_id, phase, evidence);
        append_with_receipt(&receipt, || self.journal.record(phase, evidence))
    }
    pub fn outcome(
        &mut self,
        outcome: &Result<MutationOutcome>,
        events: &[ExecutionReport],
    ) -> Result<()> {
        match outcome {
            Ok(outcome) => {
                self.record(
                    "outcome",
                    &json!({"outcome":outcome,"observed_events":events}),
                )?;
                if matches!(outcome, MutationOutcome::Rejected(_)) {
                    self.resolve(&json!({"rejected":true}))?;
                }
            }
            Err(_) => {
                self.record(
                    "not_sent",
                    &json!({"submitted":false,"observed_events":events}),
                )?;
                self.resolve(&json!({"submitted":false}))?;
            }
        }
        Ok(())
    }
    pub fn resolve(&mut self, evidence: &Value) -> Result<()> {
        self.record("resolved", &json!({"confirmed":true,"evidence":evidence}))
    }
}
pub(super) async fn finish<T>(
    broker: &FixBroker,
    client: &mut Client,
    journals: &mut [Mutation],
    result: Result<T>,
) -> Result<T> {
    let logout = client.logout().await;
    let events = client.take_events();
    let evidence = json!({"logout_confirmed":logout.is_ok(),"observed_events":events});
    let ids: Vec<_> = journals
        .iter()
        .map(|journal| journal.client_order_id.as_str())
        .collect();
    let logout_receipt = json!({"account_identity":account_identity(broker),"client_order_ids":ids,"phase":"logout","evidence":evidence});
    trace_receipt(&logout_receipt);
    let recorded = collect_append_results(
        journals
            .iter_mut()
            .map(|journal| journal.record("logout", &evidence)),
    );
    if recorded.is_err() || result.is_err() {
        tracing::error!(receipt=%logout_receipt,"cTrader FIX logout evidence retained after operation or journal failure; reconcile before retry");
    }
    if let Err(error) = logout {
        tracing::warn!(%error,"cTrader FIX logout failed after operation; do not replay mutation");
    }
    recorded?;
    result
}

pub(super) fn account_identity(broker: &FixBroker) -> Value {
    json!({"sender_comp_id":broker.trade.sender_comp_id,"target_comp_id":broker.trade.target_comp_id,
        "quote_host":broker.quote.host,"quote_port":broker.quote.port,"trade_host":broker.trade.host,"trade_port":broker.trade.port})
}
fn receipt(identity: &Value, id: &str, phase: &str, evidence: &Value) -> Value {
    json!({"account_identity":identity,"client_order_id":id,"phase":phase,"evidence":evidence})
}
fn trace_receipt(receipt: &Value) {
    tracing::info!(receipt=%receipt,"cTrader FIX mutation evidence receipt; preserve for reconciliation");
}
fn append_with_receipt<T>(receipt: &Value, append: impl FnOnce() -> Result<T>) -> Result<T> {
    trace_receipt(receipt);
    match append() {
        Ok(value) => Ok(value),
        Err(error) => {
            tracing::error!(%error,receipt=%receipt,"cTrader FIX journal append failed; preserve this receipt and reconcile before further mutations");
            Err(error)
        }
    }
}
fn collect_append_results(results: impl Iterator<Item = Result<()>>) -> Result<()> {
    let mut first_error = None;
    for result in results {
        if let Err(error) = result {
            tracing::error!(%error,"cTrader FIX evidence journal append failed; preserve the preceding structured receipt and reconcile before further mutations");
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;
