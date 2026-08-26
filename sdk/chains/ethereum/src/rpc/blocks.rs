use std::fmt;

use indexing::SourceError;
use json_rpc::{Config as TransportConfig, Http as HttpClient};
use serde_json::{Value, json};

use super::{
    Client, HttpConfig, Limits,
    error::{BuildError, BuildErrorKind},
    transport::{Client as JsonClient, RawJson},
    wire::{CallError, invalid_rpc_response, map_json_rpc_error, source_error},
};
use crate::{TransactionId, Wei};

pub(super) type ProductionClient = HttpClient;

/// Shared chain identity and block-level JSON-RPC methods.
pub(super) struct Methods<C> {
    pub(super) client: Client<C>,
    pub(super) expected_chain_id: u64,
    limits: Option<Limits>,
}

impl Methods<ProductionClient> {
    pub(super) fn http(config: HttpConfig) -> Result<Self, BuildError> {
        let mut transport =
            TransportConfig::new(config.endpoints[0].clone(), config.request_timeout);
        transport.endpoints = config.endpoints;
        transport.max_response_bytes = config.max_response_bytes;
        transport.headers = config.headers;
        transport.retry = config.retry_policy;
        let client = HttpClient::new(transport).map_err(|_| BuildError {
            kind: BuildErrorKind::HttpTransport,
            message: "failed to construct Ethereum RPC HTTP transport".to_owned(),
        })?;
        Self::with_client(client, config.expected_chain_id, config.limits)
    }
}

impl<C> Methods<C> {
    pub(super) fn with_client(
        client: C,
        expected_chain_id: u64,
        limits: Limits,
    ) -> Result<Self, BuildError> {
        Self::from_client(Client::new(client), expected_chain_id, Some(limits))
    }

    pub(super) fn from_client(
        client: Client<C>,
        expected_chain_id: u64,
        limits: Option<Limits>,
    ) -> Result<Self, BuildError> {
        if expected_chain_id == 0 {
            return Err(BuildError::invalid(
                "expected Ethereum chain ID must be non-zero",
            ));
        }
        Ok(Self {
            client,
            expected_chain_id,
            limits,
        })
    }

    pub(super) fn limits(&self) -> Result<&Limits, SourceError> {
        self.limits.as_ref().ok_or_else(|| {
            source_error(
                "Ethereum transaction adapter has no construction limits",
                false,
            )
        })
    }
}

impl<C> Clone for Methods<C> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            expected_chain_id: self.expected_chain_id,
            limits: self.limits.clone(),
        }
    }
}

impl<C> fmt::Debug for Methods<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Methods")
            .field("client", &"[REDACTED]")
            .field("expected_chain_id", &self.expected_chain_id)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl<C> Methods<C>
where
    C: JsonClient,
{
    pub(super) async fn verify_chain_id(&self) -> Result<(), SourceError> {
        let actual = self.chain_id().await?;
        if actual != self.expected_chain_id {
            return Err(source_error(
                format!(
                    "Ethereum RPC chain ID {actual} does not match configured chain ID {}",
                    self.expected_chain_id
                ),
                false,
            ));
        }
        Ok(())
    }

    async fn chain_id(&self) -> Result<u64, SourceError> {
        self.rpc_u64("eth_chainId", json!([])).await
    }

    pub(super) async fn rpc_u64(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<u64, SourceError> {
        let raw = self.request_result(method, params).await?;
        let value: String = raw.deserialize().map_err(map_json_rpc_error)?;
        super::wire::parse_quantity_u64(&value)
            .map_err(|message| invalid_rpc_response(method, message))
    }

    pub(super) async fn rpc_wei(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<Wei, SourceError> {
        let raw = self.request_result(method, params).await?;
        let value: String = raw.deserialize().map_err(map_json_rpc_error)?;
        super::wire::parse_quantity_wei(&value)
            .map_err(|message| invalid_rpc_response(method, message))
    }

    pub(super) async fn latest_canonical_parameter(&self) -> Result<Value, SourceError> {
        let raw = self
            .request_result("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        let block: Value = raw.deserialize().map_err(map_json_rpc_error)?;
        let hash = block.get("hash").and_then(Value::as_str).ok_or_else(|| {
            invalid_rpc_response("eth_getBlockByNumber", "latest block has no hash")
        })?;
        let hash = super::wire::parse_fixed_data::<32>(hash, "block hash")
            .map_err(|message| invalid_rpc_response("eth_getBlockByNumber", message))?;
        Ok(json!({
            "blockHash": super::wire::data_hex(&hash),
            "requireCanonical": true,
        }))
    }

    pub(super) async fn request_result(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, SourceError> {
        self.request_result_detailed(method, params)
            .await
            .map_err(|error| error.into_source(method))
    }

    pub(super) async fn request_result_detailed(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, CallError> {
        self.client.call(method, params).await
    }

    pub(super) async fn request_result_detailed_once(
        &self,
        method: &'static str,
        params: Value,
    ) -> Result<RawJson, CallError> {
        self.client.call_once(method, params).await
    }

    pub(super) async fn confirm_known_transaction(
        &self,
        expected: &TransactionId,
    ) -> Result<bool, SourceError> {
        let raw = self
            .request_result(
                "eth_getTransactionByHash",
                json!([super::wire::transaction_id_hex(expected)]),
            )
            .await?;
        let value: Value = raw.deserialize().map_err(map_json_rpc_error)?;
        if value.is_null() {
            return Ok(false);
        }
        let returned = value.get("hash").and_then(Value::as_str).ok_or_else(|| {
            invalid_rpc_response("eth_getTransactionByHash", "transaction object has no hash")
        })?;
        let returned = super::wire::parse_transaction_id(returned, "eth_getTransactionByHash")?;
        if &returned != expected {
            return Err(invalid_rpc_response(
                "eth_getTransactionByHash",
                "transaction object hash does not match the lookup",
            ));
        }
        Ok(true)
    }
}
