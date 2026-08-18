use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ErrorKind {
    UnsupportedCurve,
    UnsupportedScheme,
    UnsupportedOperation,
    InvalidRequest,
    Other,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}
