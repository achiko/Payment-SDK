use std::{error::Error, fmt};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignerError {
    pub kind: SignerErrorKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignerErrorKind {
    KeyNotFound,
    UnsupportedCurve,
    UnsupportedScheme,
    UnsupportedOperation,
    Unavailable,
    UserRejected,
    InvalidRequest,
    Other,
}

impl fmt::Display for SignerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for SignerError {}
