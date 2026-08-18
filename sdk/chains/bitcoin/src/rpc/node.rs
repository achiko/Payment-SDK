use indexing::{BlockHeight, SourceError};
use serde_json::Value;

use crate::Network;

use super::{
    Client, NodeStatus,
    error::{map_json_rpc_error, source_error},
    transport::Client as Transport,
    wire::{parse_bitcoin_block_hash, parse_object, required_bool, required_string, required_u64},
};

const CORE_31_VERSION_MINIMUM: u64 = 310_000;
const CORE_32_VERSION_MINIMUM: u64 = 320_000;

impl<C> Client<C>
where
    C: Transport,
{
    /// Revalidates identity and deployment prerequisites against live node state.
    pub async fn readiness(&self) -> Result<NodeStatus, SourceError> {
        let network_info = self
            .request_result("getnetworkinfo", serde_json::json!([]))
            .await?;
        let network_info = parse_object(&network_info, "Bitcoin getnetworkinfo result")?;
        let version = required_u64(&network_info, "version", "Bitcoin Core version")?;
        if !(CORE_31_VERSION_MINIMUM..CORE_32_VERSION_MINIMUM).contains(&version) {
            return Err(source_error(
                "Bitcoin RPC must be a Bitcoin Core 31.x node",
                false,
            ));
        }

        let chain_info = self
            .request_result("getblockchaininfo", serde_json::json!([]))
            .await?;
        let chain_info = parse_object(&chain_info, "Bitcoin getblockchaininfo result")?;
        let chain = required_string(&chain_info, "chain", "Bitcoin Core chain")?;
        let network = Network::from_core_chain_name(&chain).ok_or_else(|| {
            source_error("Bitcoin Core returned an unsupported chain name", false)
        })?;
        if network != self.connection.config.expected_network {
            return Err(source_error(
                "Bitcoin RPC network does not match configuration",
                false,
            ));
        }
        if required_bool(&chain_info, "pruned", "Bitcoin pruning status")? {
            return Err(source_error(
                "Bitcoin index source requires an unpruned node",
                false,
            ));
        }
        if required_bool(
            &chain_info,
            "initialblockdownload",
            "Bitcoin initial-block-download status",
        )? {
            return Err(source_error(
                "Bitcoin Core is still in initial block download",
                true,
            ));
        }
        let blocks = required_u64(&chain_info, "blocks", "Bitcoin block height")?;
        let headers = required_u64(&chain_info, "headers", "Bitcoin header height")?;
        if blocks != headers {
            return Err(source_error(
                "Bitcoin Core block and header heights are not synchronized",
                true,
            ));
        }
        let best_block_hash = parse_bitcoin_block_hash(&required_string(
            &chain_info,
            "bestblockhash",
            "Bitcoin best block hash",
        )?)?;

        let index_info = self
            .request_result("getindexinfo", serde_json::json!(["txindex"]))
            .await?;
        let index_info = parse_object(&index_info, "Bitcoin getindexinfo result")?;
        let txindex = index_info
            .get("txindex")
            .and_then(Value::as_object)
            .ok_or_else(|| source_error("Bitcoin Core transaction index is not enabled", false))?;
        if !required_bool(txindex, "synced", "Bitcoin transaction-index status")? {
            return Err(source_error(
                "Bitcoin Core transaction index is not synchronized",
                true,
            ));
        }
        if let Some(index_height) = txindex.get("best_block_height") {
            let index_height = index_height.as_u64().ok_or_else(|| {
                source_error(
                    "Bitcoin transaction-index height is not an unsigned integer",
                    true,
                )
            })?;
            if index_height != blocks {
                return Err(source_error(
                    "Bitcoin Core transaction index has not reached the canonical tip",
                    true,
                ));
            }
        }

        let genesis = self
            .request_result("getblockhash", serde_json::json!([0]))
            .await?;
        let genesis: String = genesis.deserialize().map_err(map_json_rpc_error)?;
        let genesis = parse_bitcoin_block_hash(&genesis)?;
        if genesis != self.connection.config.expected_genesis_hash {
            return Err(source_error(
                "Bitcoin RPC genesis hash does not match configuration",
                false,
            ));
        }

        Ok(NodeStatus {
            version,
            network,
            height: BlockHeight(blocks),
            best_block_hash,
        })
    }
}
