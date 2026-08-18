use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use storage::Error;

use super::support::{invalid_request, unavailable};

pub(super) fn normalized_absolute_path(path: &Path) -> Result<PathBuf, Error> {
    if path.as_os_str().is_empty() {
        return Err(invalid_request("RocksDB path must not be empty"));
    }
    if !path.is_absolute() {
        return Err(invalid_request(
            "RocksDB path must be absolute; resolve application paths at composition",
        ));
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(invalid_request("RocksDB path escapes the filesystem root"));
                }
            }
        }
    }
    Ok(normalized)
}

pub(super) fn resolved_path_for_overlap(path: &Path) -> Result<PathBuf, Error> {
    let ancestor = path
        .ancestors()
        .find(|ancestor| ancestor.exists())
        .ok_or_else(|| invalid_request("RocksDB path has no existing filesystem ancestor"))?;
    let canonical_ancestor = fs::canonicalize(ancestor).map_err(|error| {
        unavailable(format!(
            "failed to resolve RocksDB path for overlap validation: {error}"
        ))
    })?;
    let suffix = path.strip_prefix(ancestor).map_err(|error| {
        invalid_request(format!(
            "failed to separate RocksDB path from its existing ancestor: {error}"
        ))
    })?;
    Ok(canonical_ancestor.join(suffix))
}

pub(super) fn validate_separate_paths(
    first: &Path,
    second: &Path,
    first_label: &str,
    second_label: &str,
) -> Result<(), Error> {
    if first == second || first.starts_with(second) || second.starts_with(first) {
        return Err(invalid_request(format!(
            "RocksDB {first_label} and {second_label} paths must not overlap"
        )));
    }
    Ok(())
}
