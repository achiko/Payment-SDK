use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{Error, ErrorKind};

use super::Config;

/// Shared owner for the one configured Solana RPC transport.
///
/// Native method adapters added later clone this owner rather than construct
/// per-feature transports or endpoint lists.
pub struct Client<C> {
    pub(super) inner: Arc<C>,
}

impl Client<json_rpc::Http> {
    pub fn connect(config: Config) -> Result<Self, Error> {
        let transport = json_rpc::Http::new(config.into_transport()).map_err(map_transport)?;
        Ok(Self::new(transport))
    }
}

impl<C> Client<C>
where
    C: json_rpc::Client,
{
    pub(super) async fn request<T>(&self, method: &'static str, params: Value) -> Result<T, Error>
    where
        T: DeserializeOwned,
    {
        let result = self
            .inner
            .request_once(method, params)
            .await
            .map_err(map_transport)?;
        let raw = result.map_err(|failure| {
            Error::new(
                ErrorKind::RpcRemote(failure.code),
                format!("Solana RPC {method} failed with code {}", failure.code),
            )
        })?;
        raw.deserialize::<T>().map_err(|_| {
            Error::new(
                ErrorKind::MalformedRpc,
                format!("Solana RPC {method} returned malformed data"),
            )
        })
    }

    /// Executes one call at an already-declared submission wire boundary.
    ///
    /// The caller supplies the identifier derived from the exact locally
    /// signed envelope. Every failure after dispatch preserves that local ID
    /// and remains unknown; provider output never becomes authority.
    pub(super) async fn request_after_dispatch<T>(
        &self,
        method: &'static str,
        params: Value,
        local_id: base::TransactionId,
    ) -> Result<T, base::TransactionError>
    where
        T: DeserializeOwned,
    {
        self.request(method, params).await.map_err(|_| {
            base::TransactionError::new(
                base::TransactionErrorKind::Unknown,
                "Solana submission outcome is unknown",
            )
            .with_ambiguous_transaction_id(local_id)
        })
    }
}

fn map_transport(error: json_rpc::Error) -> Error {
    let kind = match error.kind {
        json_rpc::ErrorKind::InvalidConfiguration | json_rpc::ErrorKind::InvalidRequest => {
            ErrorKind::InvalidRpcConfiguration
        }
        json_rpc::ErrorKind::Timeout => ErrorKind::RpcTimeout,
        json_rpc::ErrorKind::Unavailable => ErrorKind::RpcUnavailable,
        json_rpc::ErrorKind::HttpStatus(status) => ErrorKind::RpcHttpStatus(status),
        json_rpc::ErrorKind::ResponseTooLarge => ErrorKind::ResponseTooLarge,
        json_rpc::ErrorKind::InvalidResponse => ErrorKind::MalformedRpc,
    };
    Error::new(kind, "Solana RPC request failed")
}

impl<C> Client<C> {
    #[must_use]
    pub fn new(transport: C) -> Self {
        Self {
            inner: Arc::new(transport),
        }
    }
}

impl<C> Clone for Client<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use json_rpc::{BoxFuture, Call, CallResult, Failure, RawJson};
    use serde_json::json;

    use super::*;

    #[derive(Clone)]
    enum Outcome {
        Local(json_rpc::ErrorKind),
        Remote(i64),
    }

    struct Failing(Outcome);

    impl json_rpc::Client for Failing {
        fn request<'a>(
            &'a self,
            method: &'a str,
            params: Value,
        ) -> BoxFuture<'a, Result<CallResult, json_rpc::Error>> {
            self.request_once(method, params)
        }

        fn request_once<'a>(
            &'a self,
            _method: &'a str,
            _params: Value,
        ) -> BoxFuture<'a, Result<CallResult, json_rpc::Error>> {
            Box::pin(async move {
                match self.0 {
                    Outcome::Local(kind) => Err(json_rpc::Error {
                        kind,
                        message: "provider secret".into(),
                    }),
                    Outcome::Remote(code) => Ok(Err(Failure {
                        code,
                        message: "untrusted provider prose".into(),
                        data: Some(
                            RawJson::from_serializable(&json!({"secret":"hidden"})).unwrap(),
                        ),
                    })),
                }
            })
        }

        fn batch<'a>(
            &'a self,
            _calls: Vec<Call>,
        ) -> BoxFuture<'a, Result<Vec<CallResult>, json_rpc::Error>> {
            Box::pin(async { unreachable!("Solana uses one-shot requests") })
        }
    }

    #[test]
    fn clones_share_one_transport_owner() {
        let client = Client::new(String::from("one endpoint"));
        let clone = client.clone();
        assert!(Arc::ptr_eq(&client.inner, &clone.inner));
    }

    #[tokio::test]
    async fn maps_every_transport_and_resource_class_without_provider_prose() {
        for (source, expected) in [
            (
                json_rpc::ErrorKind::InvalidConfiguration,
                ErrorKind::InvalidRpcConfiguration,
            ),
            (
                json_rpc::ErrorKind::InvalidRequest,
                ErrorKind::InvalidRpcConfiguration,
            ),
            (json_rpc::ErrorKind::Timeout, ErrorKind::RpcTimeout),
            (json_rpc::ErrorKind::Unavailable, ErrorKind::RpcUnavailable),
            (
                json_rpc::ErrorKind::HttpStatus(429),
                ErrorKind::RpcHttpStatus(429),
            ),
            (
                json_rpc::ErrorKind::ResponseTooLarge,
                ErrorKind::ResponseTooLarge,
            ),
            (
                json_rpc::ErrorKind::InvalidResponse,
                ErrorKind::MalformedRpc,
            ),
        ] {
            let error = Client::new(Failing(Outcome::Local(source)))
                .request::<u64>("method", json!([]))
                .await
                .unwrap_err();
            assert_eq!(error.kind(), expected);
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[tokio::test]
    async fn preserves_remote_code_without_trusting_message_or_data() {
        let error = Client::new(Failing(Outcome::Remote(-32_002)))
            .request::<u64>("getSlot", json!([]))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RpcRemote(-32_002));
        assert_eq!(
            error.to_string(),
            "Solana RPC getSlot failed with code -32002"
        );
        assert!(!error.to_string().contains("provider"));
        assert!(!error.to_string().contains("hidden"));
    }

    #[tokio::test]
    async fn post_dispatch_failures_preserve_only_the_local_ambiguous_id() {
        for outcome in [
            Outcome::Local(json_rpc::ErrorKind::Timeout),
            Outcome::Local(json_rpc::ErrorKind::InvalidResponse),
            Outcome::Remote(-32_002),
        ] {
            let local = base::TransactionId::new("local-first-signature");
            let error = Client::new(Failing(outcome))
                .request_after_dispatch::<String>("sendTransaction", json!([]), local.clone())
                .await
                .unwrap_err();
            assert_eq!(error.kind, base::TransactionErrorKind::Unknown);
            assert_eq!(error.ambiguous_transaction_id, Some(local));
            assert!(!error.to_string().contains("provider"));
        }
    }
}
