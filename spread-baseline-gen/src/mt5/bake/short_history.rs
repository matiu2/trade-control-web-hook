//! Explicit recent-history supplementation, retaining each source's actual bounds.
use std::collections::BTreeSet;

use color_eyre::{Result, eyre::ensure};

use super::super::Report;

/// Fill exactly the failed symbols of a fully attempted 90-day report.
/// This opt-in path requires five calendar days and the normal quality checks;
/// its generated provenance labels the shorter estimates as provisional.
pub fn render(
    primary: &Report,
    recent: &Report,
    account: &str,
    login: u64,
    server: &str,
) -> Result<String> {
    validate_catalogue(primary, recent)?;
    super::validate_report(recent, account, login, server, 5)?;
    let measured = Report {
        requested_symbols: primary
            .instruments
            .iter()
            .map(|r| r.symbol.clone())
            .collect(),
        complete: true,
        failures: vec![],
        ..primary.clone()
    };
    let table = super::render(&measured, account, login, server)?;
    let (header, body) = table
        .split_once("#[rustfmt::skip]")
        .ok_or_else(|| color_eyre::eyre::eyre!("generated MT5 table is missing its declaration"))?;
    let body = body
        .strip_suffix("];\n")
        .ok_or_else(|| color_eyre::eyre::eyre!("generated MT5 table is incomplete"))?;
    let provenance = provenance(recent);
    let rows = recent
        .instruments
        .iter()
        .map(|row| super::render_row(row, account))
        .collect::<Result<Vec<_>>>()?
        .join("");
    Ok(format!(
        "{header}// Mixed coverage: 90-day primary profiles plus explicit shorter history.\n\
         {provenance}#[rustfmt::skip]{body}{rows}];\n"
    ))
}

fn validate_catalogue(primary: &Report, recent: &Report) -> Result<()> {
    let wanted = primary.requested_symbols.iter().collect::<BTreeSet<_>>();
    let measured = primary.instruments.iter().map(|r| &r.symbol);
    let failed = primary
        .failures
        .iter()
        .map(|r| &r.symbol)
        .collect::<BTreeSet<_>>();
    let attempted = measured
        .chain(failed.iter().copied())
        .collect::<BTreeSet<_>>();
    let replacements = recent.requested_symbols.iter().collect::<BTreeSet<_>>();
    ensure!(
        wanted.len() == primary.requested_symbols.len()
            && failed.len() == primary.failures.len()
            && attempted.len() == primary.instruments.len() + primary.failures.len()
            && wanted == attempted,
        "primary calibration has unattempted or duplicate instruments"
    );
    ensure!(
        !failed.is_empty() && replacements == failed,
        "short-history report must cover exactly the primary report's failed symbols"
    );
    Ok(())
}

fn provenance(recent: &Report) -> String {
    recent
        .instruments
        .iter()
        .map(|row| {
            format!(
                "// Short-history {}: requested {} through {}; observed {} through {}; {} M1 observations; provisional.\n",
                row.symbol,
                recent.from_utc.to_rfc3339(),
                recent.to_utc.to_rfc3339(),
                row.first_minute_utc.to_rfc3339(),
                row.last_minute_utc.to_rfc3339(),
                row.profile.n_bars,
            )
        })
        .collect()
}
