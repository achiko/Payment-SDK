use std::{error::Error as StdError, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidConfiguration,
    InvalidRequest,
    Timeout,
    Unavailable,
    HttpStatus(u16),
    InvalidResponse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Timeout
                | ErrorKind::Unavailable
                | ErrorKind::HttpStatus(429 | 502 | 503 | 504)
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {}
