use std::{error::Error, fmt};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainError {
    pub kind: ChainErrorKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainErrorKind {
    InvalidAddress,
    InvalidTransaction,
    InsufficientFunds,
    FeeUnavailable,
    RpcUnavailable,
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
