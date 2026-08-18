use std::{error, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Unsupported,
    Duplicate,
    Generation,
    InvalidSecret,
    InvalidAddress,
    InvalidAmount,
    AddressMismatch,
    Balance,
    History,
    Transaction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for Error {}
