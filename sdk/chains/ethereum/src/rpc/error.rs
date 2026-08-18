use std::{error::Error, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildErrorKind {
    InvalidConfiguration,
    HttpTransport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildError {
    pub kind: BuildErrorKind,
    pub message: String,
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for BuildError {}

impl BuildError {
    pub(super) fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: BuildErrorKind::InvalidConfiguration,
            message: message.into(),
        }
    }
}
