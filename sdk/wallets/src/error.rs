use std::{error, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Unsupported,
    Duplicate,
    NotFound,
    Conflict,
    Unavailable,
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

impl From<base::TransactionError> for Error {
    fn from(error: base::TransactionError) -> Self {
        let kind = match error.kind {
            base::TransactionErrorKind::InvalidAddress => ErrorKind::InvalidAddress,
            base::TransactionErrorKind::InvalidAmount => ErrorKind::InvalidAmount,
            base::TransactionErrorKind::Unavailable
            | base::TransactionErrorKind::Unknown
            | base::TransactionErrorKind::Timeout => ErrorKind::Unavailable,
            base::TransactionErrorKind::InvalidSnapshot
            | base::TransactionErrorKind::InvalidTransaction
            | base::TransactionErrorKind::Unsupported
            | base::TransactionErrorKind::InsufficientFunds
            | base::TransactionErrorKind::Fee
            | base::TransactionErrorKind::Signing
            | base::TransactionErrorKind::Divergent
            | base::TransactionErrorKind::Rejected => ErrorKind::Transaction,
        };
        Self::new(kind, error.message)
    }
}

impl From<indexing::IndexError> for Error {
    fn from(error: indexing::IndexError) -> Self {
        let kind = match error.kind {
            indexing::IndexErrorKind::Conflict => ErrorKind::Conflict,
            indexing::IndexErrorKind::ScopeMismatch | indexing::IndexErrorKind::InvalidRequest => {
                ErrorKind::Unsupported
            }
            indexing::IndexErrorKind::Source
            | indexing::IndexErrorKind::Store
            | indexing::IndexErrorKind::InvalidBlock
            | indexing::IndexErrorKind::CannotConnect
            | indexing::IndexErrorKind::ReorgTooDeep => ErrorKind::Unavailable,
        };
        Self::new(kind, error.message)
    }
}
