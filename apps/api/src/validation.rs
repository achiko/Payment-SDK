use std::collections::BTreeSet;

use super::{AnyError, Chain, Config, RpcConfig, SyncConfig, ethereum_hash, ethereum_limits};

pub(super) fn validate(config: &Config) -> Result<(), AnyError> {
    if config.bearer_token_env.trim().is_empty() {
        return Err("bearer-token environment name must not be empty".into());
    }
    if config.indexes.bitcoin.is_none() && config.indexes.ethereum.is_none() {
        return Err("at least one chain must be configured".into());
    }
    let mut ids = BTreeSet::new();
    for wallet in &config.wallets {
        if wallet.id.trim().is_empty() || wallet.secret_env.trim().is_empty() {
            return Err(
                "configured wallet ID and secret environment name must not be empty".into(),
            );
        }
        if !ids.insert(&wallet.id) {
            return Err("configured wallet IDs must not contain duplicates".into());
        }
        let missing_chain = match wallet.chain {
            Chain::Bitcoin => config.indexes.bitcoin.is_none(),
            Chain::Ethereum => config.indexes.ethereum.is_none(),
        };
        if missing_chain {
            return Err("configured wallet references a chain that is not configured".into());
        }
    }
    if let Some(value) = &config.indexes.bitcoin {
        validate_common(&value.database, &value.rpc, &value.sync)?;
        let hash = chain_bitcoin::parse_bitcoin_block_hash(&value.genesis_hash)?;
        if chain_bitcoin::format_bitcoin_block_hash(&hash)? != value.genesis_hash {
            return Err(
                "Bitcoin genesis hash must use canonical lowercase display encoding".into(),
            );
        }
    }
    if let Some(value) = &config.indexes.ethereum {
        validate_common(&value.database, &value.rpc, &value.sync)?;
        if value.network.trim().is_empty() || value.chain_id == 0 {
            return Err("Ethereum network and chain ID must be configured".into());
        }
        ethereum_hash(&value.genesis_hash)?;
        ethereum_limits(&value.limits)?;
    }
    Ok(())
}

fn validate_common(
    database: &std::path::Path,
    rpc: &RpcConfig,
    sync: &SyncConfig,
) -> Result<(), AnyError> {
    if database.as_os_str().is_empty() {
        return Err("index database path must not be empty".into());
    }
    if rpc.endpoints.is_empty()
        || rpc.endpoints.iter().any(|value| value.trim().is_empty())
        || rpc.timeout_seconds == 0
        || rpc.max_response_bytes == 0
        || rpc.headers.iter().any(|(name, _)| name.trim().is_empty())
    {
        return Err("invalid RPC configuration".into());
    }
    if sync.confirmation_depth == 0
        || sync.reorg_retention == 0
        || sync.poll_millis == 0
        || sync.batch_size == 0
    {
        return Err(
            "confirmation depth, reorg retention, poll interval, and batch size must be positive"
                .into(),
        );
    }
    Ok(())
}
