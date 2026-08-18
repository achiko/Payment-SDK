use std::{error::Error as StdError, fmt, future::Future};

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    #[must_use]
    pub fn post(endpoint: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: "POST".to_owned(),
            endpoint: endpoint.into(),
            headers: Vec::new(),
            body,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Timeout,
    Unavailable,
    Rejected,
    InvalidResponse,
    Other,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {}

/// Minimal asynchronous request executor, analogous to Go's `Do` boundary.
pub trait Client: Send + Sync {
    fn execute<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Response, Error>>;
}
