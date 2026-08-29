use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidBatch,
    InvalidBudget,
    InvalidIdentity,
    InvalidSecret,
    Generation,
    Signing,
    InvalidRpcConfiguration,
    RpcTimeout,
    RpcUnavailable,
    RpcHttpStatus(u16),
    RpcRemote(i64),
    MalformedRpc,
    ResponseTooLarge,
    BelowFloor,
    UnsupportedDestination,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}
