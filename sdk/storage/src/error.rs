use std::{error::Error, fmt};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageError {
    pub kind: StorageErrorKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageErrorKind {
    Conflict,
    Unavailable,
    CorruptData,
    InvalidRequest,
    Other,
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for StorageError {}
