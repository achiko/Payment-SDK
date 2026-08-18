use std::{future::Future, pin::Pin};

use serde_json::Value;

use crate::{Call, CallResult, Error};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Minimal execution boundary used by chain adapters and deterministic doubles.
pub trait Client: Send + Sync {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, std::result::Result<CallResult, Error>>;

    fn batch<'a>(
        &'a self,
        calls: Vec<Call>,
    ) -> BoxFuture<'a, std::result::Result<Vec<CallResult>, Error>>;
}
