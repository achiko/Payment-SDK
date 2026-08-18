//! Bounded JSON-RPC execution over `jsonrpsee`.
//!
//! This crate owns endpoint policy only. `jsonrpsee` owns JSON-RPC framing,
//! request correlation, response validation, and batch ordering.

mod client;
mod error;
mod http;
mod value;

#[cfg(test)]
mod http_test;

use serde_json::Value;

pub use client::{BoxFuture, Client};
pub use error::{Error, ErrorKind};
pub use http::{Config, Http, Retry};
pub use value::{CallResult, Failure, RawJson};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub method: String,
    pub params: Value,
}

impl Call {
    #[must_use]
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            method: method.into(),
            params,
        }
    }
}
