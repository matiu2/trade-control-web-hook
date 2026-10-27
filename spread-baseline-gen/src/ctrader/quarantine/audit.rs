//! Synchronously durable evidence: an exclusion is accepted only after fsync.
use super::persistence::atomic_write;
pub(super) use super::persistence::read;
use super::{AuditEvidence, Binding, Minute};
use color_eyre::{
    Result,
    eyre::{Context, ensure},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub binding: Binding,
    pub records: Vec<Minute>,
}

pub(crate) struct Audit {
    pub(super) binding: Binding,
    pub(super) path: PathBuf,
    pub(super) records: Vec<Minute>,
    _lock: File,
}
impl Audit {
    pub(crate) fn open(path: &Path, resume: Option<&Path>, binding: Binding) -> Result<Self> {
        binding.validate()?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()?;
        let path = parent.join(
            path.file_name()
                .ok_or_else(|| color_eyre::eyre::eyre!("audit has no filename"))?,
        );
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path.with_extension("json.lock"))?;
        lock.try_lock()
            .wrap_err("quarantine audit is already in use")?;
        let records = resume_records(&path, resume, &binding)?;
        let audit = Self {
            binding,
            path,
            records,
            _lock: lock,
        };
        audit.validate_records(&audit.records)?;
        audit.persist(&audit.records)?;
        Ok(audit)
    }
    pub(crate) fn minutes(&self, symbol: &str, id: i64) -> Result<Vec<Minute>> {
        let mut rows = self
            .records
            .iter()
            .filter(|r| r.symbol == symbol)
            .cloned()
            .collect::<Vec<_>>();
        ensure!(
            rows.iter().all(|r| r.symbol_id == id),
            "quarantine native symbol identity changed for {symbol}"
        );
        rows.sort_by_key(|row| row.from_utc);
        Ok(rows)
    }
    pub(crate) fn evidence(&self) -> Result<AuditEvidence> {
        let (manifest, sha256) = read(&self.path)?;
        ensure!(
            manifest.binding == self.binding && manifest.records == self.records,
            "quarantine audit changed while collecting"
        );
        Ok(AuditEvidence {
            path: self.path.clone(),
            sha256,
            excluded_minutes: self.records.len(),
        })
    }
    pub(crate) fn record(&mut self, minute: Minute) -> Result<()> {
        minute.validate()?;
        if let Some(previous) = self
            .records
            .iter()
            .find(|r| r.symbol == minute.symbol && r.from_utc == minute.from_utc)
        {
            ensure!(
                previous.symbol_id == minute.symbol_id,
                "quarantine native symbol identity changed"
            );
            return Ok(());
        }
        let mut next = self.records.clone();
        next.push(minute);
        next.sort_by(|a, b| (&a.symbol, a.from_utc).cmp(&(&b.symbol, b.from_utc)));
        self.validate_records(&next)?;
        self.persist(&next)?;
        self.records = next;
        Ok(())
    }
    fn validate_records(&self, rows: &[Minute]) -> Result<()> {
        validate_records(&self.binding, rows)
    }
    fn persist(&self, records: &[Minute]) -> Result<()> {
        let value = Manifest {
            binding: self.binding.clone(),
            records: records.to_vec(),
        };
        atomic_write(&self.path, &serde_json::to_vec_pretty(&value)?)
    }
}

fn resume_records(path: &Path, resume: Option<&Path>, binding: &Binding) -> Result<Vec<Minute>> {
    let load = |path: &Path| -> Result<Vec<Minute>> {
        let manifest = read(path)?.0;
        ensure!(
            manifest.binding == *binding,
            "quarantine audit identity, policy, scope or sampling window changed"
        );
        Ok(manifest.records)
    };
    let source = resume.map(load).transpose()?;
    let target = path.try_exists()?.then(|| load(path)).transpose()?;
    match (target, source) {
        (Some(target), Some(source)) => {
            ensure!(
                source.iter().all(|record| target.contains(record)),
                "existing target quarantine audit would discard or alter authoritative resume exclusions"
            );
            Ok(target)
        }
        (Some(records), None) | (None, Some(records)) => Ok(records),
        (None, None) => Ok(vec![]),
    }
}

pub(super) fn validate_records(binding: &Binding, rows: &[Minute]) -> Result<()> {
    binding.validate()?;
    let mut seen = std::collections::BTreeSet::new();
    let mut counts = std::collections::BTreeMap::new();
    let mut ids = std::collections::BTreeMap::new();
    for row in rows {
        row.validate()?;
        ensure!(
            binding.requested_symbols.contains(&row.symbol)
                && row.from_utc >= binding.from_utc
                && row.to_utc <= binding.to_utc,
            "quarantine exclusion is outside its bound window or instrument scope"
        );
        ensure!(
            seen.insert((&row.symbol, row.from_utc)),
            "duplicate quarantined minute"
        );
        ensure!(
            ids.insert(&row.symbol, row.symbol_id)
                .is_none_or(|id| id == row.symbol_id),
            "quarantine native instrument identity changed"
        );
        let count = counts.entry(&row.symbol).or_insert(0u32);
        *count += 1;
        ensure!(
            *count <= binding.policy.max_minutes_per_instrument,
            "crossed-minute quarantine budget exhausted for {} (maximum {})",
            row.symbol,
            binding.policy.max_minutes_per_instrument
        );
    }
    ensure!(
        rows.windows(2)
            .all(|pair| (&pair[0].symbol, pair[0].from_utc) < (&pair[1].symbol, pair[1].from_utc)),
        "quarantine records must be in canonical instrument/minute order"
    );
    Ok(())
}

pub(crate) fn path(report: &Path) -> PathBuf {
    report.with_extension("quarantine.json")
}
