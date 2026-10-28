use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};

pub(super) fn lock(path: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let path = path.with_extension("json.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    // SAFETY: flock borrows a valid descriptor, held until the caller drops File.
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "Another TradeNation bake holds this file lock"
    );
    Ok(file)
}
use std::{collections::BTreeMap, io::Write, path::Path};

pub(super) fn atomic_write(path: &Path, text: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(text.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

pub(super) fn merge_tables(existing: &str, incoming: &str) -> Result<String> {
    let (header, old) = parse(existing)?;
    let (_, new) = parse(incoming)?;
    let mut rows = old;
    for (key, line) in new {
        ensure!(
            key.0 == "tradenation",
            "Cannot replace another broker's rows"
        );
        rows.insert(key, line);
    }
    Ok(format!(
        "{header}\n{}\n];\n",
        rows.into_values().collect::<Vec<_>>().join("\n")
    ))
}

type Rows<'a> = BTreeMap<(String, String), &'a str>;

fn parse(source: &str) -> Result<(&str, Rows<'_>)> {
    let (header, body) = source
        .split_once(" = &[\n")
        .ok_or_else(|| eyre!("Unrecognized spread table declaration"))?;
    ensure!(
        header.contains("pub static SPREAD_BASELINE_CANDLE:"),
        "Unexpected spread table"
    );
    let body = body
        .strip_suffix("];\n")
        .ok_or_else(|| eyre!("Unrecognized spread table terminator"))?;
    let mut rows = BTreeMap::new();
    for line in body.lines() {
        let body = line
            .trim()
            .strip_prefix('(')
            .ok_or_else(|| eyre!("Invalid spread row"))?;
        ensure!(line.trim().ends_with("),"), "Invalid row terminator");
        let (broker, rest) = quoted(body)?;
        let rest = rest
            .trim_start()
            .strip_prefix(',')
            .ok_or_else(|| eyre!("Invalid row separator"))?
            .trim_start();
        let (symbol, _) = quoted(rest)?;
        ensure!(
            rows.insert((broker, symbol), line).is_none(),
            "Duplicate spread key"
        );
    }
    Ok((&source[..header.len() + " = &[".len()], rows))
}

fn quoted(source: &str) -> Result<(String, &str)> {
    let mut decoder = serde_json::Deserializer::from_str(source).into_iter::<String>();
    let value = decoder
        .next()
        .transpose()?
        .ok_or_else(|| eyre!("Missing row key"))?;
    Ok((value, &source[decoder.byte_offset()..]))
}
