//! Generic JSON-RPC framing. Concrete chain methods do not belong here.

use std::{error::Error, fmt};
use transport::BoxFuture;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestId {
    Number(u64),
    String(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawJson(pub Vec<u8>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonRpcRequest {
    pub id: RequestId,
    pub method: String,
    pub params: RawJson,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonRpcResponse {
    pub id: RequestId,
    pub result: Result<RawJson, JsonRpcFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonRpcFailure {
    pub code: i64,
    pub message: String,
    pub data: Option<RawJson>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonRpcError {
    pub message: String,
}

impl fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for JsonRpcError {}

pub trait JsonRpcClient: Send + Sync {
    fn request<'a>(
        &'a self,
        request: JsonRpcRequest,
    ) -> BoxFuture<'a, Result<JsonRpcResponse, JsonRpcError>>;

    fn batch<'a>(
        &'a self,
        requests: Vec<JsonRpcRequest>,
    ) -> BoxFuture<'a, Result<Vec<JsonRpcResponse>, JsonRpcError>>;
}
