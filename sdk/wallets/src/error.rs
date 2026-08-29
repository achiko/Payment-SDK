use std::{error, fmt};

use base::TransactionId;

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
    InvalidBatch,
    AddressMismatch,
    Balance,
    History,
    Transaction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    /// Canonical local ID preserved from a concrete transaction-layer ambiguity.
    pub ambiguous_transaction_id: Option<TransactionId>,
}

impl Error {
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            ambiguous_transaction_id: None,
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
        let base::TransactionError {
            kind,
            message,
            ambiguous_transaction_id,
        } = error;
        let kind = match kind {
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
        Self {
            kind,
            message,
            ambiguous_transaction_id,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_wallet_error_has_no_ambiguous_transaction_id() {
        let error = Error::new(
            ErrorKind::Unavailable,
            "provider claimed transaction canonical-id",
        );

        assert_eq!(error.ambiguous_transaction_id, None);
    }

    #[test]
    fn ordinary_transaction_conversion_does_not_synthesize_ambiguity() {
        let error = Error::from(base::TransactionError::new(
            base::TransactionErrorKind::Unknown,
            "provider claimed transaction canonical-id",
        ));

        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(error.message, "provider claimed transaction canonical-id");
        assert_eq!(error.ambiguous_transaction_id, None);
    }

    #[test]
    fn transaction_error_conversion_preserves_ambiguous_transaction_id() {
        let id = base::TransactionId::new("canonical-id");
        let error = Error::from(
            base::TransactionError::new(
                base::TransactionErrorKind::Timeout,
                "submission outcome is unknown",
            )
            .with_ambiguous_transaction_id(id.clone()),
        );

        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(error.message, "submission outcome is unknown");
        assert_eq!(error.to_string(), "submission outcome is unknown");
        assert_eq!(error.ambiguous_transaction_id, Some(id));
    }
}
