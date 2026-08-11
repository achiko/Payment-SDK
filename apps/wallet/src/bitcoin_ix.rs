//! Bounded, authentication-mode-aware client for the Bitcoin IX UTXO projection.

use std::{fmt, net::IpAddr, time::Duration};

use chain_bitcoin::{
    BitcoinAddress, BitcoinNetwork, BitcoinRpcUtxo, BitcoinTransactionId, BitcoinUtxoSet,
    BitcoinUtxoSource, BoxFuture, Satoshi, parse_bitcoin_block_hash,
};
use http_support::AuthenticationMode;
use indexing::{BlockHeight, BlockRef, SourceError};
use reqwest::{
    StatusCode, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;

#[derive(Clone)]
pub struct BitcoinIxClientConfig {
    pub endpoint: String,
    pub authentication_mode: AuthenticationMode,
    pub headers: Vec<(String, String)>,
    pub request_timeout: Duration,
    pub maximum_response_bytes: usize,
    pub page_size: usize,
    pub maximum_pages_per_address: usize,
    pub retry_attempts: u32,
}

impl fmt::Debug for BitcoinIxClientConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = self
            .headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        formatter
            .debug_struct("BitcoinIxClientConfig")
            .field("endpoint", &"[REDACTED]")
            .field("authentication_mode", &self.authentication_mode)
            .field("header_names", &names)
            .field("request_timeout", &self.request_timeout)
            .field("maximum_response_bytes", &self.maximum_response_bytes)
            .field("page_size", &self.page_size)
            .field("maximum_pages_per_address", &self.maximum_pages_per_address)
            .field("retry_attempts", &self.retry_attempts)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinIxReadiness {
    pub network: String,
    pub phase: String,
    pub checkpoint: BlockRef,
    pub confirmation_depth: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IxProjectionSnapshot {
    generation: u64,
    revision: u64,
    checkpoint: BlockRef,
}

#[derive(Clone)]
pub struct BitcoinIxClient {
    client: reqwest::Client,
    endpoint: Url,
    headers: HeaderMap,
    network: BitcoinNetwork,
    config: BitcoinIxClientConfig,
}

impl fmt::Debug for BitcoinIxClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BitcoinIxClient")
            .field("endpoint", &"[REDACTED]")
            .field("network", &self.network)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl BitcoinIxClient {
    pub fn new(
        network: BitcoinNetwork,
        config: BitcoinIxClientConfig,
    ) -> Result<Self, SourceError> {
        validate_limits(&config)?;
        let endpoint = validate_endpoint(&config.endpoint)?;
        let mut headers = HeaderMap::new();
        for (name, value) in &config.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| source("Bitcoin IX header name is invalid", false))?;
            if config.authentication_mode == AuthenticationMode::GlobalTrusted
                && name == reqwest::header::AUTHORIZATION
            {
                continue;
            }
            if headers.contains_key(&name) {
                return Err(source("Bitcoin IX header names must be unique", false));
            }
            let value = HeaderValue::from_str(value)
                .map_err(|_| source("Bitcoin IX header value is invalid", false))?;
            headers.insert(name, value);
        }
        if config.authentication_mode == AuthenticationMode::Strict
            && !headers.contains_key(reqwest::header::AUTHORIZATION)
        {
            return Err(source(
                "Bitcoin IX requires exactly one authorization header",
                false,
            ));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.request_timeout)
            .build()
            .map_err(|_| source("could not construct Bitcoin IX HTTP client", false))?;
        Ok(Self {
            client,
            endpoint,
            headers,
            network,
            config,
        })
    }

    pub async fn readiness(&self) -> Result<BitcoinIxReadiness, SourceError> {
        self.require_ready_authentication_mode().await?;
        let response: StatusDto = self
            .get_json(
                &[
                    "v1",
                    "scopes",
                    "bitcoin",
                    self.network.canonical_name(),
                    "status",
                ],
                &[],
            )
            .await?;
        if response.scope.chain != "bitcoin"
            || response.scope.network != self.network.canonical_name()
        {
            return Err(source(
                "Bitcoin IX readiness returned the wrong chain or network",
                false,
            ));
        }
        if response.phase != "ready" {
            return Err(source("Bitcoin IX is not in the ready phase", true));
        }
        let checkpoint = response
            .checkpoint
            .ok_or_else(|| source("Bitcoin IX ready status has no checkpoint", true))?;
        let checkpoint = parse_block(checkpoint)?;
        let confirmation_depth =
            canonical_u64(&response.confirmation_depth, "IX confirmation depth")?;
        if confirmation_depth == 0 {
            return Err(source(
                "Bitcoin IX confirmation depth must be greater than zero",
                false,
            ));
        }
        Ok(BitcoinIxReadiness {
            network: response.scope.network,
            phase: response.phase,
            checkpoint,
            confirmation_depth,
        })
    }

    async fn address_utxos(
        &self,
        address: &BitcoinAddress,
        expected_snapshot: &mut Option<IxProjectionSnapshot>,
    ) -> Result<Vec<BitcoinRpcUtxo>, SourceError> {
        self.require_ready_authentication_mode().await?;
        let mut after = None;
        let mut outputs = Vec::new();
        for _ in 0..self.config.maximum_pages_per_address {
            let limit = self.config.page_size.to_string();
            let mut query = vec![("limit", limit.as_str())];
            if let Some(cursor) = after.as_deref() {
                query.push(("after", cursor));
            }
            let response: UtxoPageDto = self
                .get_json(
                    &[
                        "v1",
                        "scopes",
                        "bitcoin",
                        self.network.canonical_name(),
                        "addresses",
                        &address.0,
                        "utxos",
                    ],
                    &query,
                )
                .await?;
            accept_snapshot(
                expected_snapshot,
                &response.generation,
                &response.revision,
                response.checkpoint,
            )?;
            for output in response.outputs {
                if output.address != address.0 {
                    return Err(source(
                        "Bitcoin IX returned a UTXO owned by a different address",
                        true,
                    ));
                }
                // Parse and retain creation height even though the current
                // chain trait exposes confirmations. This rejects malformed
                // height facts before they reach spendability policy.
                canonical_u64(&output.created_height, "IX UTXO creation height")?;
                outputs.push(BitcoinRpcUtxo {
                    transaction_id: output
                        .transaction_id
                        .parse::<BitcoinTransactionId>()
                        .map_err(|_| source("Bitcoin IX returned an invalid txid", true))?
                        .0,
                    output_index: canonical_u32(&output.output_index, "IX UTXO output index")?,
                    value: Satoshi(canonical_u64(&output.value_sats, "IX UTXO value")?),
                    script_pubkey: canonical_hex(&output.script_pubkey)?,
                    confirmations: canonical_u64(&output.confirmations, "IX UTXO confirmations")?,
                    coinbase: output.coinbase,
                });
            }
            match response.next {
                Some(next) if !next.is_empty() => after = Some(next),
                None => return Ok(outputs),
                Some(_) => return Err(source("Bitcoin IX returned an empty cursor", true)),
            }
        }
        Err(source(
            "Bitcoin IX UTXO pagination exceeded the configured page limit",
            false,
        ))
    }

    async fn require_ready_authentication_mode(&self) -> Result<(), SourceError> {
        let health: HealthDto = self.get_json(&["health", "ready"], &[]).await?;
        validate_authentication_mode(self.config.authentication_mode, &health.authentication_mode)?;
        if health.status != "ready" {
            return Err(source("Bitcoin IX is not operationally ready", true));
        }
        Ok(())
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &[&str],
        query: &[(&str, &str)],
    ) -> Result<T, SourceError> {
        let mut url = self.endpoint.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| source("Bitcoin IX endpoint cannot accept path segments", false))?;
            segments.pop_if_empty();
            segments.extend(path.iter().copied());
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }

        let mut last = None;
        for attempt in 0..self.config.retry_attempts {
            let result = self
                .client
                .get(url.clone())
                .headers(self.headers.clone())
                .send()
                .await;
            match result {
                Ok(response) if response.status().is_success() => {
                    let bytes = bounded_body(response, self.config.maximum_response_bytes).await?;
                    return serde_json::from_slice(&bytes)
                        .map_err(|_| source("Bitcoin IX returned invalid JSON", true));
                }
                Ok(response) => {
                    let retryable = retryable_status(response.status());
                    let failure = source(
                        format!("Bitcoin IX returned HTTP {}", response.status().as_u16()),
                        retryable,
                    );
                    if !retryable {
                        return Err(failure);
                    }
                    last = Some(failure);
                }
                Err(error) => {
                    let retryable = error.is_timeout() || error.is_connect() || error.is_body();
                    let failure = source("Bitcoin IX request failed", retryable);
                    if !retryable {
                        return Err(failure);
                    }
                    last = Some(failure);
                }
            }
            if attempt + 1 < self.config.retry_attempts {
                let factor = 1_u64.checked_shl(attempt.min(6)).unwrap_or(64);
                tokio::time::sleep(Duration::from_millis(50 * factor)).await;
            }
        }
        Err(last.unwrap_or_else(|| source("Bitcoin IX request was not attempted", false)))
    }
}

impl BitcoinUtxoSource for BitcoinIxClient {
    fn utxos<'a>(
        &'a self,
        addresses: Vec<BitcoinAddress>,
    ) -> BoxFuture<'a, Result<BitcoinUtxoSet, SourceError>> {
        Box::pin(async move {
            let mut outputs = Vec::new();
            let mut snapshot = None;
            for address in addresses {
                outputs.extend(self.address_utxos(&address, &mut snapshot).await?);
            }
            let snapshot = snapshot.ok_or_else(|| {
                source(
                    "Bitcoin IX UTXO lookup requires at least one address",
                    false,
                )
            })?;
            Ok(BitcoinUtxoSet {
                checkpoint: snapshot.checkpoint,
                outputs,
            })
        })
    }
}

async fn bounded_body(
    mut response: reqwest::Response,
    maximum: usize,
) -> Result<Vec<u8>, SourceError> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(source(
            "Bitcoin IX response exceeds the configured limit",
            true,
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| source("Bitcoin IX response body failed", true))?
    {
        if bytes.len().saturating_add(chunk.len()) > maximum {
            return Err(source(
                "Bitcoin IX response exceeds the configured limit",
                true,
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn validate_limits(config: &BitcoinIxClientConfig) -> Result<(), SourceError> {
    if config.request_timeout.is_zero()
        || config.maximum_response_bytes == 0
        || config.page_size == 0
        || config.page_size > 1_000
        || config.maximum_pages_per_address == 0
        || config.retry_attempts == 0
    {
        return Err(source(
            "Bitcoin IX limits must be positive and page size must not exceed 1000",
            false,
        ));
    }
    Ok(())
}

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::CONFLICT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn validate_endpoint(value: &str) -> Result<Url, SourceError> {
    let url = Url::parse(value).map_err(|_| source("Bitcoin IX endpoint is invalid", false))?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(source(
            "Bitcoin IX endpoint must have no credentials, query, or fragment",
            false,
        ));
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" if is_loopback(&url) => Ok(url),
        "http" => Err(source(
            "non-loopback Bitcoin IX endpoint requires HTTPS",
            false,
        )),
        _ => Err(source("Bitcoin IX endpoint must use HTTP or HTTPS", false)),
    }
}

fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn canonical_u64(value: &str, field: &str) -> Result<u64, SourceError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| source(format!("{field} is invalid"), true))?;
    if parsed.to_string() != value {
        return Err(source(format!("{field} is not canonical"), true));
    }
    Ok(parsed)
}

fn accept_snapshot(
    expected: &mut Option<IxProjectionSnapshot>,
    generation: &str,
    revision: &str,
    checkpoint: Option<BlockDto>,
) -> Result<(), SourceError> {
    let snapshot = IxProjectionSnapshot {
        generation: canonical_u64(generation, "IX projection generation")?,
        revision: canonical_u64(revision, "IX projection revision")?,
        checkpoint: parse_block(
            checkpoint.ok_or_else(|| source("Bitcoin IX UTXO page has no checkpoint", true))?,
        )?,
    };
    match expected {
        Some(current) if *current != snapshot => Err(source(
            "Bitcoin IX projection snapshot changed during the UTXO read",
            true,
        )),
        None => {
            *expected = Some(snapshot);
            Ok(())
        }
        Some(_) => Ok(()),
    }
}

fn parse_block(block: BlockDto) -> Result<BlockRef, SourceError> {
    let height = BlockHeight(canonical_u64(&block.height, "IX checkpoint height")?);
    let hash = parse_bitcoin_block_hash(&block.hash)?;
    let parent_hash = block
        .parent_hash
        .map(|value| parse_bitcoin_block_hash(&value))
        .transpose()?;
    if (height.0 == 0) != parent_hash.is_none() {
        return Err(source(
            "Bitcoin IX checkpoint parent hash is inconsistent with its height",
            true,
        ));
    }
    let timestamp = block
        .timestamp
        .map(|value| canonical_u64(&value, "IX checkpoint timestamp"))
        .transpose()?;
    Ok(BlockRef {
        height,
        hash,
        parent_hash,
        timestamp,
    })
}

fn canonical_u32(value: &str, field: &str) -> Result<u32, SourceError> {
    let value = canonical_u64(value, field)?;
    u32::try_from(value).map_err(|_| source(format!("{field} exceeds u32"), true))
}

fn canonical_hex(value: &str) -> Result<Vec<u8>, SourceError> {
    let hexadecimal = value
        .strip_prefix("0x")
        .ok_or_else(|| source("Bitcoin IX script must have a 0x prefix", true))?;
    if hexadecimal.is_empty() || hexadecimal.len() % 2 != 0 {
        return Err(source("Bitcoin IX script has an invalid byte length", true));
    }
    let decoded = hex::decode(hexadecimal)
        .map_err(|_| source("Bitcoin IX script contains invalid hexadecimal", true))?;
    if format!("0x{}", hex::encode(&decoded)) != value {
        return Err(source(
            "Bitcoin IX script is not canonical lowercase hex",
            true,
        ));
    }
    Ok(decoded)
}

fn source(message: impl Into<String>, retryable: bool) -> SourceError {
    SourceError {
        message: message.into(),
        retryable,
    }
}

fn validate_authentication_mode(
    expected: AuthenticationMode,
    reported: &str,
) -> Result<(), SourceError> {
    if expected.as_str() == reported {
        Ok(())
    } else {
        Err(source(
            "Bitcoin IX authentication mode does not match client configuration",
            false,
        ))
    }
}

#[derive(Deserialize)]
struct HealthDto {
    authentication_mode: String,
    status: String,
}

#[derive(Deserialize)]
struct StatusDto {
    scope: ScopeDto,
    phase: String,
    checkpoint: Option<BlockDto>,
    confirmation_depth: String,
}

#[derive(Deserialize)]
struct ScopeDto {
    chain: String,
    network: String,
}

#[derive(Clone, Deserialize)]
struct BlockDto {
    height: String,
    hash: String,
    parent_hash: Option<String>,
    timestamp: Option<String>,
}

#[derive(Deserialize)]
struct UtxoPageDto {
    generation: String,
    revision: String,
    checkpoint: Option<BlockDto>,
    outputs: Vec<UtxoDto>,
    next: Option<String>,
}

#[derive(Deserialize)]
struct UtxoDto {
    transaction_id: String,
    output_index: String,
    value_sats: String,
    script_pubkey: String,
    address: String,
    created_height: String,
    coinbase: bool,
    confirmations: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(endpoint: &str) -> BitcoinIxClientConfig {
        BitcoinIxClientConfig {
            endpoint: endpoint.to_owned(),
            authentication_mode: AuthenticationMode::Strict,
            headers: vec![("authorization".to_owned(), "Bearer hidden".to_owned())],
            request_timeout: Duration::from_secs(1),
            maximum_response_bytes: 1024,
            page_size: 100,
            maximum_pages_per_address: 2,
            retry_attempts: 1,
        }
    }

    #[test]
    fn client_rejects_insecure_or_credentialed_endpoints() {
        assert!(
            BitcoinIxClient::new(BitcoinNetwork::Regtest, config("http://ix.example.test"))
                .is_err()
        );
        assert!(
            BitcoinIxClient::new(
                BitcoinNetwork::Regtest,
                config("https://user:password@ix.example.test")
            )
            .is_err()
        );
        BitcoinIxClient::new(BitcoinNetwork::Regtest, config("http://127.0.0.1:8081"))
            .expect("loopback plaintext must be accepted");

        let mut oversized_page = config("https://ix.example.test");
        oversized_page.page_size = 1_001;
        assert!(BitcoinIxClient::new(BitcoinNetwork::Regtest, oversized_page).is_err());
    }

    #[test]
    fn client_rejects_duplicate_and_crlf_headers() {
        let mut duplicate = config("https://ix.example.test");
        duplicate
            .headers
            .push(("Authorization".to_owned(), "second".to_owned()));
        assert!(BitcoinIxClient::new(BitcoinNetwork::Mainnet, duplicate).is_err());

        let mut injected = config("https://ix.example.test");
        injected.headers[0].1 = "Bearer hidden\r\nX-Evil: yes".to_owned();
        assert!(BitcoinIxClient::new(BitcoinNetwork::Mainnet, injected).is_err());

        let mut missing = config("https://ix.example.test");
        missing.headers.clear();
        assert!(BitcoinIxClient::new(BitcoinNetwork::Mainnet, missing).is_err());

        let mut global = config("https://ix.example.test");
        global.authentication_mode = AuthenticationMode::GlobalTrusted;
        global.headers[0].1 = "ignored\r\ninvalid".to_owned();
        BitcoinIxClient::new(BitcoinNetwork::Mainnet, global)
            .expect("global-trusted client must ignore Authorization headers");
    }

    #[test]
    fn readiness_authentication_mode_comparison_fails_closed() {
        validate_authentication_mode(AuthenticationMode::Strict, "strict")
            .expect("matching strict mode must pass");
        validate_authentication_mode(AuthenticationMode::GlobalTrusted, "global_trusted")
            .expect("matching global-trusted mode must pass");
        let error = validate_authentication_mode(AuthenticationMode::Strict, "global_trusted")
            .expect_err("mode mismatch must fail");
        assert!(!error.retryable);
    }

    #[test]
    fn canonical_wire_values_are_strict() {
        assert_eq!(canonical_u64("42", "value").expect("canonical"), 42);
        assert_eq!(canonical_u64("0", "value").expect("canonical zero"), 0);
        assert!(canonical_u64("042", "value").is_err());
        assert_eq!(canonical_hex("0x0011").expect("canonical"), vec![0, 17]);
        assert!(canonical_hex("0xAA").is_err());
        assert!(retryable_status(StatusCode::CONFLICT));
        assert!(!retryable_status(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn projection_snapshot_is_bound_across_every_page_and_address() {
        let checkpoint = || {
            Some(BlockDto {
                height: "42".to_owned(),
                hash: "11".repeat(32),
                parent_hash: Some("10".repeat(32)),
                timestamp: Some("1000".to_owned()),
            })
        };
        let mut snapshot = None;
        accept_snapshot(&mut snapshot, "7", "9", checkpoint()).expect("first page binds snapshot");
        accept_snapshot(&mut snapshot, "7", "9", checkpoint())
            .expect("same snapshot remains valid");
        let error = accept_snapshot(&mut snapshot, "7", "10", checkpoint())
            .expect_err("revision movement must abort the stitched read");
        assert!(error.retryable);
    }

    #[test]
    fn route_paths_append_to_an_optional_endpoint_prefix() {
        let client = BitcoinIxClient::new(
            BitcoinNetwork::Regtest,
            config("https://ix.example.test/internal"),
        )
        .expect("prefixed HTTPS endpoint must be valid");
        let mut url = client.endpoint.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .expect("HTTP URL must accept path segments");
            segments.pop_if_empty();
            segments.extend(["v1", "scopes", "bitcoin", "regtest", "status"]);
        }
        assert_eq!(
            url.as_str(),
            "https://ix.example.test/internal/v1/scopes/bitcoin/regtest/status"
        );
    }
}
