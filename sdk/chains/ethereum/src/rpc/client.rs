use std::sync::Arc;

use indexing::SourceError;
use serde_json::Value;

use super::transport::{Call, Client as JsonClient, Failure, RawJson};
pub(crate) use super::wire::CallError;
use super::wire::map_json_rpc_error;

/// Shared Ethereum JSON-RPC execution boundary.
///
/// This type translates generic call results into chain errors. `jsonrpsee`
/// owns framing, correlation, and batch ordering; account, block, and
/// transaction semantics stay in their focused adapters.
pub struct Client<C> {
    inner: Arc<ClientState<C>>,
}

struct ClientState<C> {
    transport: C,
}

impl<C> Clone for Client<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<C> Client<C> {
    #[must_use]
    pub fn new(transport: C) -> Self {
        Self {
            inner: Arc::new(ClientState { transport }),
        }
    }
}

impl<C> Client<C>
where
    C: JsonClient,
{
    pub(crate) async fn call(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, CallError> {
        let result = self
            .inner
            .transport
            .request(method, params)
            .await
            .map_err(map_json_rpc_error)
            .map_err(CallError::Local)?;
        result.map_err(CallError::Remote)
    }

    /// Executes one state-changing RPC attempt without hidden failover.
    pub(crate) async fn call_once(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, CallError> {
        let result = self
            .inner
            .transport
            .request_once(method, params)
            .await
            .map_err(map_json_rpc_error)
            .map_err(CallError::Local)?;
        result.map_err(CallError::Remote)
    }

    /// Executes calls as one JSON-RPC batch and returns results in input order.
    pub(crate) async fn batch(
        &self,
        calls: Vec<(&'static str, Value)>,
    ) -> Result<Vec<Result<RawJson, Failure>>, SourceError> {
        let requests = calls
            .into_iter()
            .map(|(method, params)| Call::new(method, params))
            .collect();
        self.inner
            .transport
            .batch(requests)
            .await
            .map_err(map_json_rpc_error)
    }
}
