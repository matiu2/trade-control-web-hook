//! Exclude only the requested reports and their process-generated sidecars.
use super::*;
use color_eyre::eyre::eyre;

pub(super) fn normalize(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    paths
        .iter()
        .map(|path| {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Ok(parent.canonicalize()?.join(
                path.file_name()
                    .ok_or_else(|| eyre!("artifact needs a filename"))?,
            ))
        })
        .collect()
}

pub(super) fn contains(artifacts: &[PathBuf], path: &Path) -> bool {
    artifacts.iter().any(|artifact| {
        path == artifact
            || path == artifact.with_extension("json.lock")
            || path == artifact.with_extension("finish.lock")
            || temporary(artifact, path)
    })
}

fn temporary(artifact: &Path, path: &Path) -> bool {
    if artifact.parent() != path.parent() {
        return false;
    }
    let Some(stem) = artifact.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.strip_prefix(&format!("{stem}.json."))
                .and_then(|suffix| suffix.strip_suffix(".tmp"))
                .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()))
        })
}
