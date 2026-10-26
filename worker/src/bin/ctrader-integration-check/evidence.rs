use color_eyre::{Result, eyre::ensure};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

pub struct Report {
    path: PathBuf,
    _lock: File,
    pub value: Value,
}
impl Report {
    pub fn create(path: &Path, account: &str, instrument: &str, execute: bool) -> Result<Self> {
        ensure!(
            path.extension().is_some_and(|e| e == "json"),
            "report must be a JSON path"
        );
        let reservation = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        reservation.sync_all()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path.with_extension("json.lock"))?;
        lock.try_lock()?;
        let report = Self {
            path: path.into(),
            _lock: lock,
            value: json!({"version":1,"account":account,"instrument":instrument,"execute":execute,"process_id":std::process::id(),"audit_client_prefix":format!("manage-{}-",std::process::id()),"started_at":chrono::Utc::now(),"status":"preflight","steps":[]}),
        };
        report.save()?;
        Ok(report)
    }
    pub fn step(&mut self, name: &str, evidence: Value) -> Result<()> {
        self.value["steps"]
            .as_array_mut()
            .ok_or_else(|| color_eyre::eyre::eyre!("invalid report state"))?
            .push(json!({"at":chrono::Utc::now(),"step":name,"evidence":evidence}));
        self.save()
    }
    pub fn save(&self) -> Result<()> {
        let temporary = self
            .path
            .with_extension(format!("json.{}.tmp", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(&self.value)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, &self.path)?;
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()?;
        Ok(())
    }
    pub fn finish(&mut self, status: &str) -> Result<()> {
        self.value["status"] = json!(status);
        self.value["updated_at"] = json!(chrono::Utc::now());
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn report_is_private_checkpointed_and_never_overwrites_existing() {
        crate::test_logging();
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("report.json");
        let mut r = Report::create(&p, "demo", "EURUSD", false).unwrap();
        r.step("native", json!({"minimum":1000})).unwrap();
        r.finish("planned").unwrap();
        let v: Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["steps"].as_array().unwrap().len(), 1);
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(Report::create(&p, "demo", "EURUSD", true).is_err());
    }
}
