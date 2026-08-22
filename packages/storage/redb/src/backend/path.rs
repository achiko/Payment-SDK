use std::{
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use storage::Error;

use super::support::{invalid_request, unavailable};

pub(super) struct DatabasePath {
    pub(super) path: PathBuf,
    pub(super) initialize: bool,
}

pub(super) fn validated_database_path(path: &Path) -> Result<DatabasePath, Error> {
    let normalized = normalized_absolute_path(path)?;
    let file_name = normalized
        .file_name()
        .ok_or_else(|| invalid_request("redb path must identify a database file"))?;
    let parent = normalized
        .parent()
        .ok_or_else(|| invalid_request("redb database file must have a parent directory"))?;
    let parent_metadata = fs::metadata(parent).map_err(|error| match error.kind() {
        ErrorKind::NotFound => invalid_request("redb database parent directory does not exist"),
        _ => unavailable(format!(
            "failed to inspect redb database parent directory: {error}"
        )),
    })?;
    if !parent_metadata.is_dir() {
        return Err(invalid_request(
            "redb database parent path must be a directory",
        ));
    }
    let canonical_parent = fs::canonicalize(parent).map_err(|error| {
        unavailable(format!(
            "failed to resolve redb database parent directory: {error}"
        ))
    })?;
    let resolved = canonical_parent.join(file_name);

    let initialize = match fs::metadata(&resolved) {
        Ok(metadata) if metadata.is_dir() => {
            return Err(invalid_request(
                "redb database path must be a file, not a directory",
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(invalid_request(
                "redb database path must identify a regular file",
            ));
        }
        Ok(metadata) => metadata.len() == 0,
        Err(error) if error.kind() == ErrorKind::NotFound => true,
        Err(error) => {
            return Err(unavailable(format!(
                "failed to inspect redb database file: {error}"
            )));
        }
    };

    Ok(DatabasePath {
        path: resolved,
        initialize,
    })
}

fn normalized_absolute_path(path: &Path) -> Result<PathBuf, Error> {
    if path.as_os_str().is_empty() {
        return Err(invalid_request("redb path must not be empty"));
    }
    if !path.is_absolute() {
        return Err(invalid_request(
            "redb path must be absolute; resolve application paths at composition",
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
                    return Err(invalid_request("redb path escapes the filesystem root"));
                }
            }
        }
    }
    Ok(normalized)
}
