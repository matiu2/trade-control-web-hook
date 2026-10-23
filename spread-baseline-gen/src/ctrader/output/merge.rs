//! Additive promotion never rewrites existing measured row text.
use color_eyre::{Result, eyre::ensure};
use std::collections::BTreeSet;

mod format;

pub(super) fn preserves(previous: &str, account: &str, incoming: &str) -> Result<()> {
    let new = format::parse(incoming)?;
    ensure!(
        new.identity.profile == account,
        "cTrader table belongs to another account"
    );
    if previous
        .lines()
        .any(|line| line.trim_start().starts_with('('))
    {
        let old = format::parse(previous)?;
        ensure!(
            old.identity == new.identity,
            "cTrader bake cannot change an existing account identity"
        );
        let incoming_keys = new
            .rows
            .iter()
            .map(|row| row.key.as_str())
            .collect::<BTreeSet<_>>();
        ensure!(
            old.rows
                .iter()
                .all(|row| incoming_keys.contains(row.key.as_str())),
            "cTrader bake refuses to erase existing symbols; include every existing profile or use --merge for new symbols"
        );
    }
    Ok(())
}

pub(super) fn tables(previous: &str, account: &str, incoming: &str) -> Result<String> {
    let old = format::parse(previous)?;
    let new = format::parse(incoming)?;
    ensure!(
        old.identity == new.identity && new.identity.profile == account,
        "additive cTrader bake requires the same verified demo account identity"
    );
    ensure!(
        old.window == new.window,
        "additive cTrader bake requires the same sampling window"
    );
    let existing = old
        .rows
        .iter()
        .map(|row| row.key.as_str())
        .collect::<BTreeSet<_>>();
    ensure!(
        new.rows
            .iter()
            .all(|row| !existing.contains(row.key.as_str())),
        "additive cTrader bake refuses to replace an existing symbol"
    );
    let rows = new
        .rows
        .iter()
        .map(|row| format!("{}\n", row.line))
        .collect::<String>();
    Ok(format!(
        "{}{}{}",
        &previous[..old.insert_at],
        rows,
        &previous[old.insert_at..]
    ))
}

#[cfg(test)]
mod tests;
