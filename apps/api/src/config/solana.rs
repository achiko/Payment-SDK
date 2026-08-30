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
        chain_solana::NativeAsset::new(self.network.as_str())?;
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
