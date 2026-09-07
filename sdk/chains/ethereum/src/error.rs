use std::{error::Error, fmt};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainError {
    pub kind: ChainErrorKind,
    pub message: String,
}

impl ChainError {
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
