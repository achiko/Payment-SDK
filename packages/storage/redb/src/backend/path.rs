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

// design-lint: allow single-use-free-function -- complete lexical path normalization with root-escape checks stays separate from filesystem canonicalization and database-file validation
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

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn rejects_lexical_errors_before_inspecting_the_filesystem() {
        let directory = TempDir::new().expect("temporary directory");
        let absolute = directory.path().canonicalize().expect("absolute directory");
        let root = absolute.ancestors().last().expect("filesystem root");
        for (path, message) in [
            (PathBuf::new(), "redb path must not be empty"),
            (
                PathBuf::from("missing/database.redb"),
                "redb path must be absolute; resolve application paths at composition",
            ),
            (
                root.join("../missing/database.redb"),
                "redb path escapes the filesystem root",
            ),
            (
                root.to_path_buf(),
                "redb path must identify a database file",
            ),
        ] {
            let error = validated_database_path(&path).err().expect("invalid path");
            assert_eq!(error.kind, storage::ErrorKind::InvalidRequest);
            assert_eq!(error.message, message);
        }
    }

    #[test]
    fn normalizes_dot_and_parent_components_before_checking_the_parent() {
        let directory = TempDir::new().expect("temporary directory");
        let input = directory.path().join("missing/.././database.redb");
        let validated = validated_database_path(&input).expect("normalized path");
        assert_eq!(
            validated.path,
            directory
                .path()
                .canonicalize()
                .expect("canonical parent")
                .join("database.redb"),
        );
        assert!(validated.initialize);
        assert!(!directory.path().join("missing").exists());
        assert!(!validated.path.exists());
    }

    #[test]
    fn initialization_depends_on_file_existence_and_length() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("database.redb");
        assert!(
            validated_database_path(&path)
                .expect("absent file")
                .initialize
        );

        fs::write(&path, []).expect("empty file");
        assert!(
            validated_database_path(&path)
                .expect("empty file")
                .initialize
        );

        fs::write(&path, b"existing file").expect("nonempty file");
        assert!(
            !validated_database_path(&path)
                .expect("existing file")
                .initialize
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolves_parent_symlinks_after_lexical_parent_removal() {
        let directory = TempDir::new().expect("temporary directory");
        let parent = directory.path().canonicalize().expect("canonical parent");
        let nested = parent.join("actual/nested");
        fs::create_dir_all(&nested).expect("nested directory");
        let link = parent.join("linked");
        std::os::unix::fs::symlink(&nested, &link).expect("parent symlink");

        let direct =
            validated_database_path(&link.join("database.redb")).expect("symlink parent resolves");
        assert_eq!(direct.path, nested.join("database.redb"));
        assert!(direct.initialize);

        let traversed = validated_database_path(&link.join("../database.redb"))
            .expect("lexical parent resolves");
        assert_eq!(traversed.path, parent.join("database.redb"));
        assert!(traversed.initialize);
    }

    #[cfg(unix)]
    #[test]
    fn follows_file_symlink_metadata_without_replacing_its_path() {
        let directory = TempDir::new().expect("temporary directory");
        let parent = directory.path().canonicalize().expect("canonical parent");
        let target = parent.join("existing.redb");
        fs::write(&target, b"existing file").expect("symlink target");
        let link = parent.join("database.redb");
        std::os::unix::fs::symlink(&target, &link).expect("file symlink");

        let validated = validated_database_path(&link).expect("file symlink resolves");
        assert_eq!(validated.path, link);
        assert!(!validated.initialize);
    }
}
