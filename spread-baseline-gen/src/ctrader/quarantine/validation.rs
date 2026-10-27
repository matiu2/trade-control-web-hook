use super::{Binding, Report, audit};
use color_eyre::{Result, eyre::ensure};

pub(super) fn validate_report(report: &Report, verify_audit: bool) -> Result<()> {
    let Some(_) = &report.quarantine_policy else {
        ensure!(
            report.quarantine_audit.is_none()
                && report
                    .instruments
                    .iter()
                    .all(|r| r.quarantined_minutes.is_empty()),
            "crossed-minute exclusions require an explicit quarantine policy"
        );
        return Ok(());
    };
    let binding = Binding::from_report(report)?;
    for row in &report.instruments {
        audit::validate_records(&binding, &row.quarantined_minutes)?;
        super::validate_fraction(
            row.measurement.profile.n_bars,
            row.quarantined_minutes.len(),
        )?;
        ensure!(
            row.quarantined_minutes
                .iter()
                .all(|m| m.symbol == row.measurement.symbol && m.symbol_id == row.symbol_id),
            "quarantined minute is assigned to the wrong instrument"
        );
    }
    let evidence = report.quarantine_audit.as_ref().ok_or_else(|| {
        color_eyre::eyre::eyre!("quarantine report is missing its durable audit evidence")
    })?;
    ensure!(
        evidence.path.is_absolute(),
        "quarantine audit path must be absolute"
    );
    let (manifest, digest) = audit::read(&evidence.path)?;
    let original = &manifest.binding;
    ensure!(
        original.version == binding.version
            && original.identity == binding.identity
            && original.from_utc == binding.from_utc
            && original.to_utc == binding.to_utc
            && original.policy == binding.policy
            && binding
                .requested_symbols
                .iter()
                .all(|s| original.requested_symbols.contains(s)),
        "quarantine audit does not match the report identity, sampling window, scope or policy"
    );
    for row in &report.instruments {
        let expected = manifest
            .records
            .iter()
            .filter(|m| m.symbol == row.measurement.symbol)
            .cloned()
            .collect::<Vec<_>>();
        ensure!(
            expected == row.quarantined_minutes,
            "completed instrument is missing durable crossed-minute exclusions"
        );
    }
    if verify_audit {
        ensure!(
            evidence.sha256 == digest && evidence.excluded_minutes == manifest.records.len(),
            "quarantine evidence digest or exclusion count changed"
        );
    } else {
        // A crash may leave a newer durable audit for an unfinished symbol. Completed rows
        // must still match above; the generator adopts the newer evidence on resume.
        ensure!(
            evidence.excluded_minutes <= manifest.records.len(),
            "quarantine audit lost exclusions since the checkpoint"
        );
    }
    Ok(())
}
