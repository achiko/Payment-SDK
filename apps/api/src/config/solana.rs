use std::time::Duration;

use serde::Deserialize;

use super::AnyError;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SolanaConfig {
    network: String,
    genesis_hash: String,
    rpc: RpcConfig,
    sync: SyncConfig,
}

impl SolanaConfig {
    pub(super) fn validate(&self) -> Result<(), AnyError> {
        chain_solana::WalletConfig::new(self.network.as_str(), chain_solana::AssetKind::Native)?;
        self.genesis_hash.parse::<chain_solana::GenesisHash>()?;
        self.rpc.validate()?;
        self.sync.validate()?;
        Ok(())
    }

    pub(crate) fn network(&self) -> &str {
        &self.network
    }

    pub(crate) fn genesis_hash(&self) -> &str {
        &self.genesis_hash
    }

    pub(crate) fn rpc(&self) -> Result<chain_solana::RpcConfig, AnyError> {
        let mut config = chain_solana::RpcConfig::new(
            self.rpc.endpoint.clone(),
            Duration::from_secs(self.rpc.timeout_seconds),
            MAX_REQUEST_BYTES,
            self.rpc.max_response_bytes,
        )?;
        for (name, value) in &self.rpc.headers {
            config = config.with_header(name.clone(), value.clone());
        }
        Ok(config)
    }

    pub(crate) const fn confirmation_depth(&self) -> u64 {
        self.sync.confirmation_depth
    }

    pub(crate) const fn reorg_retention(&self) -> u64 {
        self.sync.reorg_retention
    }

    pub(crate) const fn batch_size(&self) -> usize {
        self.sync.batch_size
    }

    pub(super) const fn poll_millis(&self) -> u64 {
        self.sync.poll_millis
    }
}

// design-lint: allow duplicate-entity-base -- this closed wire schema requires a singular endpoint and explicit bounds; the Bitcoin/Ethereum failover schema uses ordered endpoints and defaults its bounds
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcConfig {
    endpoint: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
    timeout_seconds: u64,
    max_response_bytes: usize,
}

impl RpcConfig {
    fn validate(&self) -> Result<(), AnyError> {
        if self.endpoint.trim().is_empty()
            || self.timeout_seconds == 0
            || self.max_response_bytes == 0
            || self.headers.iter().any(|(name, _)| name.trim().is_empty())
        {
            return Err("invalid singular Solana RPC configuration".into());
        }
        Ok(())
    }
}

// design-lint: allow duplicate-entity-base -- this closed wire schema requires every synchronization field; Bitcoin/Ethereum default polling and batch size, so sharing Deserialize would change accepted input
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncConfig {
    confirmation_depth: u64,
    reorg_retention: u64,
    poll_millis: u64,
    batch_size: usize,
}

impl SyncConfig {
    fn validate(&self) -> Result<(), AnyError> {
        if self.confirmation_depth == 0
            || self.reorg_retention == 0
            || self.poll_millis == 0
            || self.batch_size == 0
        {
            return Err("Solana synchronization bounds must be positive".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn singular_rpc_requires_bounds_and_preserves_its_closed_raw_schema() {
        for (raw, expected) in [
            (
                r#"{"endpoint":"a","max_response_bytes":8192}"#,
                "missing field `timeout_seconds`",
            ),
            (
                r#"{"endpoint":"a","timeout_seconds":1}"#,
                "missing field `max_response_bytes`",
            ),
            (
                r#"{"endpoint":"a","endpoint":"b","timeout_seconds":1,"max_response_bytes":8192}"#,
                "duplicate field `endpoint`",
            ),
            (
                r#"{"endpoint":"a","timeout_seconds":1,"timeout_seconds":2,"max_response_bytes":8192}"#,
                "duplicate field `timeout_seconds`",
            ),
            (
                r#"{"endpoint":"a","endpoints":["b"],"timeout_seconds":1,"max_response_bytes":8192}"#,
                "unknown field `endpoints`",
            ),
            (
                r#"{"endpoint":"a","timeout_seconds":null,"max_response_bytes":8192}"#,
                "invalid type: null",
            ),
            (
                r#"{"endpoint":["a"],"timeout_seconds":1,"max_response_bytes":8192}"#,
                "invalid type: sequence",
            ),
        ] {
            serde_json::from_str::<Value>(raw).expect("syntactically valid raw JSON");
            let error = serde_json::from_str::<RpcConfig>(raw)
                .err()
                .expect("strict singular RPC schema");
            assert!(error.to_string().starts_with(expected), "{error}");
        }
        let rpc: RpcConfig = serde_json::from_str(
            r#"{"endpoint":"a","timeout_seconds":1,"max_response_bytes":8192}"#,
        )
        .unwrap();
        assert!(rpc.headers.is_empty(), "only headers may default");
    }

    #[test]
    fn sync_requires_polling_and_batch_fields_and_rejects_raw_duplicates() {
        for (raw, expected) in [
            (
                r#"{"confirmation_depth":1,"reorg_retention":10,"batch_size":7}"#,
                "missing field `poll_millis`",
            ),
            (
                r#"{"confirmation_depth":1,"reorg_retention":10,"poll_millis":17}"#,
                "missing field `batch_size`",
            ),
            (
                r#"{"confirmation_depth":1,"reorg_retention":10,"poll_millis":17,"poll_millis":18,"batch_size":7}"#,
                "duplicate field `poll_millis`",
            ),
            (
                r#"{"confirmation_depth":1,"reorg_retention":10,"poll_millis":17,"batch_size":7,"batch_size":8}"#,
                "duplicate field `batch_size`",
            ),
            (
                r#"{"confirmation_depth":1,"reorg_retention":10,"poll_millis":17,"batch_size":7,"retry":false}"#,
                "unknown field `retry`",
            ),
            (
                r#"{"confirmation_depth":1,"reorg_retention":10,"poll_millis":null,"batch_size":7}"#,
                "invalid type: null",
            ),
            (
                r#"{"confirmation_depth":1,"reorg_retention":10,"poll_millis":17,"batch_size":"7"}"#,
                "invalid type: string",
            ),
        ] {
            serde_json::from_str::<Value>(raw).expect("syntactically valid raw JSON");
            let error = serde_json::from_str::<SyncConfig>(raw)
                .err()
                .expect("strict explicit sync schema");
            assert!(error.to_string().starts_with(expected), "{error}");
        }
    }

    #[test]
    fn configured_values_reach_runtime_and_rpc_errors_still_precede_sync_errors() {
        let mut config: SolanaConfig = serde_json::from_value(json!({
            "network": "localnet",
            "genesis_hash": "11111111111111111111111111111111",
            "rpc": {
                "endpoint": "http://127.0.0.1:8899",
                "headers": [["x-fixture", "first"], ["x-fixture", "second"]],
                "timeout_seconds": 3,
                "max_response_bytes": 8192
            },
            "sync": {"confirmation_depth":2,"reorg_retention":10,"poll_millis":17,"batch_size":7}
        }))
        .unwrap();
        config.validate().expect("explicit bounds");
        let expected = chain_solana::RpcConfig::new(
            "http://127.0.0.1:8899",
            Duration::from_secs(3),
            MAX_REQUEST_BYTES,
            8192,
        )
        .unwrap()
        .with_header("x-fixture", "first")
        .with_header("x-fixture", "second");
        assert_eq!(config.rpc().unwrap(), expected);
        assert_eq!(config.confirmation_depth(), 2);
        assert_eq!(config.reorg_retention(), 10);
        assert_eq!(config.poll_millis(), 17);
        assert_eq!(config.batch_size(), 7);

        config.rpc.timeout_seconds = 0;
        config.sync.batch_size = 0;
        assert_eq!(
            config.validate().unwrap_err().to_string(),
            "invalid singular Solana RPC configuration"
        );
        config.rpc.timeout_seconds = 3;
        assert_eq!(
            config.validate().unwrap_err().to_string(),
            "Solana synchronization bounds must be positive"
        );
    }
}
