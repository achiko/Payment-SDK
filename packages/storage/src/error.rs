use std::{error::Error as StdError, fmt};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Conflict,
    Unavailable,
    CorruptData,
    InvalidRequest,
    Other,
}

impl Error {
    /// Reports a rejected atomic write condition with its caller-supplied context.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Conflict,
            message: message.into(),
        }
    }

    /// Reports stored data that violates its expected format or invariants.
    pub fn corrupt_data(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::CorruptData,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl StdError for Error {}
