use std::{io, path::PathBuf};

pub(crate) fn load_demo_env() -> Result<(), io::Error> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env");

    match dotenvy::from_path(&path) {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io::Error::other(format!(
            "failed to load {}: {error}",
            path.display()
        ))),
    }
}
