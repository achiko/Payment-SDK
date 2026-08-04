use std::{error::Error, fmt};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceError {
    pub message: String,
    pub retryable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexError {
    pub kind: IndexErrorKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexErrorKind {
    Source,
    Storage,
    InvalidBlock,
    CannotConnect,
    ReorgBeyondRetention,
    InvalidWatch,
    Other,
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for SourceError {}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for IndexError {}
