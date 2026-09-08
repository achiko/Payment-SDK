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
    pub retryable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexErrorKind {
    Source,
    Store,
    Conflict,
    ScopeMismatch,
    InvalidBlock,
    CannotConnect,
    ReorgTooDeep,
    InvalidRequest,
}

impl IndexError {
    #[must_use]
    pub fn new(kind: IndexErrorKind, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable,
        }
    }

    pub(crate) fn cannot_connect(message: impl Into<String>) -> Self {
        Self::new(IndexErrorKind::CannotConnect, message, true)
    }

    pub(crate) fn retryable_conflict(message: impl Into<String>) -> Self {
        Self::new(IndexErrorKind::Conflict, message, true)
    }
}

impl From<SourceError> for IndexError {
    fn from(error: SourceError) -> Self {
        Self::new(IndexErrorKind::Source, error.message, error.retryable)
    }
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
