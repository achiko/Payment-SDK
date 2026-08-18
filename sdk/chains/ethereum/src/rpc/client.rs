use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use indexing::SourceError;
use serde_json::Value;

use super::transport::{Client as JsonClient, Failure, RawJson, Request, RequestId, Response};
pub(crate) use super::wire::CallError;
use super::wire::{map_json_rpc_error, source_error};

/// Shared Ethereum JSON-RPC execution boundary.
///
/// This type owns framing concerns only: monotonically increasing request IDs,
/// response-ID validation, transport-error translation, and batch ordering.
/// Account, block, and transaction semantics stay in their focused adapters.
pub struct Client<C> {
    inner: Arc<ClientState<C>>,
}

struct ClientState<C> {
    transport: C,
    next_request_id: AtomicU64,
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
            inner: Arc::new(ClientState {
                transport,
                next_request_id: AtomicU64::new(1),
            }),
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
        let id = self.request_id().map_err(CallError::Local)?;
        let request = Request::new(id.clone(), method, &params)
            .map_err(map_json_rpc_error)
            .map_err(CallError::Local)?;
        let response = self
            .inner
            .transport
            .request(request)
            .await
            .map_err(map_json_rpc_error)
            .map_err(CallError::Local)?;
        validate_response(id, response)
    }

    /// Executes calls as one JSON-RPC batch and returns results in input order.
    pub(crate) async fn batch(
        &self,
        calls: Vec<(&'static str, Value)>,
    ) -> Result<Vec<Result<RawJson, Failure>>, SourceError> {
        let mut ids = Vec::with_capacity(calls.len());
        let mut requests = Vec::with_capacity(calls.len());
        for (method, params) in calls {
            let id = self.request_id()?;
            ids.push(id.clone());
            requests.push(Request::new(id, method, &params).map_err(map_json_rpc_error)?);
        }

        let mut responses = self
            .inner
            .transport
            .batch(requests)
            .await
            .map_err(map_json_rpc_error)?;
        if responses.len() != ids.len() {
            return Err(source_error(
                "Ethereum JSON-RPC batch response count is inconsistent",
                true,
            ));
        }

        ids.into_iter()
            .map(|id| {
                let index = responses
                    .iter()
                    .position(|response| response.id == id)
                    .ok_or_else(|| {
                        source_error(
                            "Ethereum JSON-RPC batch has no response for a request ID",
                            true,
                        )
                    })?;
                Ok(responses.swap_remove(index).result)
            })
            .collect()
    }

    fn request_id(&self) -> Result<RequestId, SourceError> {
        self.inner
            .next_request_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(RequestId::Number)
            .map_err(|_| source_error("Ethereum JSON-RPC request ID space is exhausted", false))
    }
}

fn validate_response(id: RequestId, response: Response) -> Result<RawJson, CallError> {
    if response.id != id {
        return Err(CallError::Local(source_error(
            "Ethereum JSON-RPC response ID does not match its request",
            true,
        )));
    }
    response.result.map_err(CallError::Remote)
}
