use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use indexing::SourceError;
use serde_json::Value;

use super::{
    CoreConfig,
    error::{CallFailure, map_json_rpc_error, map_remote_failure, source_error},
    transport::{Client as Transport, RawJson, Request, RequestId},
};

/// Credential-free Bitcoin Core method adapter over shared JSON-RPC framing.
///
/// HTTP authentication, retry, endpoint selection, timeouts, and response limits
/// belong to the injected transport. This type owns only request correlation and
/// Bitcoin Core method semantics.
pub struct Client<C> {
    pub(super) connection: Arc<Connection<C>>,
}

pub(super) struct Connection<C> {
    pub(super) client: C,
    pub(super) config: CoreConfig,
    next_request_id: AtomicU64,
}

impl<C> Clone for Client<C> {
    fn clone(&self) -> Self {
        Self {
            connection: Arc::clone(&self.connection),
        }
    }
}

impl<C> Client<C>
where
    C: Transport,
{
    pub async fn connect(client: C, config: CoreConfig) -> Result<Self, SourceError> {
        config.validate()?;
        let core = Self {
            connection: Arc::new(Connection {
                client,
                config,
                next_request_id: AtomicU64::new(1),
            }),
        };
        core.readiness().await?;
        Ok(core)
    }

    #[must_use]
    pub fn config(&self) -> &CoreConfig {
        &self.connection.config
    }

    pub(crate) async fn request_result(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, SourceError> {
        self.request_result_detailed(method, params)
            .await
            .map_err(|failure| failure.error)
    }

    pub(crate) async fn request_optional_result(
        &self,
        method: &'static str,
        params: Value,
        missing_codes: &[i64],
    ) -> Result<Option<RawJson>, SourceError> {
        match self.request_result_detailed(method, params).await {
            Ok(result) => Ok(Some(result)),
            Err(failure)
                if failure
                    .remote_code
                    .is_some_and(|code| missing_codes.contains(&code)) =>
            {
                Ok(None)
            }
            Err(failure) => Err(failure.error),
        }
    }

    pub(super) async fn request_result_detailed(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, CallFailure> {
        let id = self.request_id().map_err(CallFailure::local)?;
        let request = Request::new(id.clone(), method, &params)
            .map_err(map_json_rpc_error)
            .map_err(CallFailure::local)?;
        let response = self
            .connection
            .client
            .request(request)
            .await
            .map_err(map_json_rpc_error)
            .map_err(CallFailure::local)?;
        if response.id != id {
            return Err(CallFailure::local(source_error(
                "Bitcoin JSON-RPC response ID does not match its request",
                true,
            )));
        }
        match response.result {
            Ok(result) => Ok(result),
            Err(failure) => Err(CallFailure {
                remote_code: Some(failure.code),
                error: map_remote_failure(failure),
            }),
        }
    }

    fn request_id(&self) -> Result<RequestId, SourceError> {
        self.connection
            .next_request_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(RequestId::Number)
            .map_err(|_| source_error("Bitcoin JSON-RPC request ID space is exhausted", false))
    }
}
