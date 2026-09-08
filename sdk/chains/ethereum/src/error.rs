use std::{error::Error, fmt};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainError {
    pub kind: ChainErrorKind,
    pub message: String,
}

impl ChainError {
    pub(crate) fn from_rpc(error: indexing::SourceError) -> Self {
        Self::new(ChainErrorKind::RpcUnavailable, error.message)
    }

    pub(crate) fn new(kind: ChainErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainErrorKind {
    InvalidAddress,
    InvalidTransaction,
    InsufficientFunds,
    FeeUnavailable,
    RpcUnavailable,
    Divergent,
    Signer,
    Rejected,
    NotFound,
    Other,
}

impl fmt::Display for ChainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ChainError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_preparation_errors_preserve_message_and_existing_unavailable_classification() {
        for retryable in [false, true] {
            let error = ChainError::from_rpc(indexing::SourceError {
                message: "fixture RPC error".to_owned(),
                retryable,
            });
            assert_eq!(error.kind, ChainErrorKind::RpcUnavailable);
            assert_eq!(error.message, "fixture RPC error");
        }
    }
}
