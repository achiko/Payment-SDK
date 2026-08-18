use std::{error::Error, fmt};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositError {
    pub kind: DepositErrorKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepositErrorKind {
    NotFound,
    Conflict,
    InvalidState,
    InvariantViolation,
    Store,
    Other,
}

impl fmt::Display for DepositError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for DepositError {}
