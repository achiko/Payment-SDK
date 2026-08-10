use chain_contract::{
    Balance, BalanceReader, Broadcaster, ChainError, ChainErrorKind, DepositAddressGenerator,
    GeneratedAddress, TransactionReader, TransactionSigner, TransferBuilder,
};
use signer::{KeyProvisioner, Signer};

use crate::{
    BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB, Bitcoin, BitcoinAddress, BitcoinAddressGenerator,
    BitcoinAsset, BitcoinBuildRequest, BitcoinGenerateAddress, BitcoinNetwork, BitcoinNodeRpc,
    BitcoinReceipt, BitcoinSignedTransaction, BitcoinTransactionCodec, BitcoinTransactionId,
    BitcoinUtxoSource, BoxFuture, Satoshi, SatoshisPerKvb, UnsignedBitcoinTransaction,
};

const COINBASE_MATURITY: u64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitcoinNodePolicy {
    pub fee_estimate_target_blocks: u16,
    pub maximum_fee_rate: SatoshisPerKvb,
}

impl BitcoinNodePolicy {
    pub fn validate(self) -> Result<Self, ChainError> {
        if self.fee_estimate_target_blocks == 0 {
            return Err(invalid_transaction(
                "Bitcoin fee-estimation target must be greater than zero",
            ));
        }
        if self.maximum_fee_rate.satoshis_per_kvb() == 0 {
            return Err(invalid_transaction(
                "Bitcoin maximum fee rate must be greater than zero",
            ));
        }
        if self.maximum_fee_rate.satoshis_per_kvb() > BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB {
            return Err(invalid_transaction(
                "Bitcoin maximum fee rate exceeds Bitcoin Core's 1 BTC/kvB limit",
            ));
        }
        Ok(self)
    }
}

/// Production stateless composition with independent Core node and IX UTXO
/// adapters. Core never scans or selects wallet outputs.
#[derive(Debug)]
pub struct BitcoinProductionWallet<N, U> {
    network: BitcoinNetwork,
    node: N,
    utxos: U,
    node_policy: BitcoinNodePolicy,
    codec: BitcoinTransactionCodec,
}

impl<N, U> BitcoinProductionWallet<N, U> {
    pub fn new(
        network: BitcoinNetwork,
        node: N,
        utxos: U,
        node_policy: BitcoinNodePolicy,
    ) -> Result<Self, ChainError> {
        Ok(Self {
            network,
            node,
            utxos,
            node_policy: node_policy.validate()?,
            codec: BitcoinTransactionCodec::new(network),
        })
    }

    #[must_use]
    pub const fn network(&self) -> BitcoinNetwork {
        self.network
    }

    #[must_use]
    pub const fn node(&self) -> &N {
        &self.node
    }

    #[must_use]
    pub const fn utxo_source(&self) -> &U {
        &self.utxos
    }
}

impl<N, U> BitcoinProductionWallet<N, U>
where
    N: BitcoinNodeRpc,
    U: BitcoinUtxoSource,
{
    async fn verified_utxos(
        &self,
        address: &BitcoinAddress,
    ) -> Result<Vec<crate::BitcoinRpcUtxo>, ChainError> {
        let canonical = BitcoinAddress::parse_for_network(&address.0, self.network)?;
        let expected_script = canonical.script_pubkey_for_network(self.network)?;
        let snapshot = self.utxos.utxos(vec![canonical]).await.map_err(rpc_error)?;
        let canonical_hash = self
            .node
            .canonical_hash(snapshot.checkpoint.height)
            .await
            .map_err(rpc_error)?;
        if canonical_hash.as_ref() != Some(&snapshot.checkpoint.hash) {
            return Err(rpc_error(indexing::SourceError {
                message: "Bitcoin IX checkpoint does not match Bitcoin Core".to_owned(),
                retryable: true,
            }));
        }
        if snapshot
            .outputs
            .iter()
            .any(|utxo| utxo.script_pubkey != expected_script.as_bytes())
        {
            return Err(invalid_transaction(
                "Bitcoin IX returned a UTXO whose script does not match the requested address",
            ));
        }
        Ok(snapshot.outputs)
    }
}

impl<N, U> DepositAddressGenerator<Bitcoin> for BitcoinProductionWallet<N, U>
where
    N: Send + Sync,
    U: Send + Sync,
{
    fn generate_address<'a>(
        &'a self,
        request: BitcoinGenerateAddress,
        keys: &'a dyn KeyProvisioner,
    ) -> BoxFuture<'a, Result<GeneratedAddress<BitcoinAddress>, ChainError>> {
        Box::pin(async move {
            if request.network != self.network {
                return Err(ChainError {
                    kind: ChainErrorKind::InvalidAddress,
                    message: "Bitcoin address request uses a different network".to_owned(),
                });
            }
            BitcoinAddressGenerator
                .generate_address(request, keys)
                .await
        })
    }
}

impl<N, U> BalanceReader<Bitcoin> for BitcoinProductionWallet<N, U>
where
    N: BitcoinNodeRpc,
    U: BitcoinUtxoSource,
{
    fn balance<'a>(
        &'a self,
        address: &'a BitcoinAddress,
        _asset: &'a BitcoinAsset,
    ) -> BoxFuture<'a, Result<Balance<Satoshi>, ChainError>> {
        Box::pin(async move {
            let mut confirmed = 0_u64;
            let mut pending = 0_u64;
            let mut spendable = 0_u64;
            for utxo in self.verified_utxos(address).await? {
                if utxo.confirmations == 0 {
                    pending = checked_sum(pending, utxo.value.0)?;
                } else {
                    confirmed = checked_sum(confirmed, utxo.value.0)?;
                    if !utxo.coinbase || utxo.confirmations >= COINBASE_MATURITY {
                        spendable = checked_sum(spendable, utxo.value.0)?;
                    }
                }
            }
            Ok(Balance {
                confirmed: Satoshi(confirmed),
                pending: Satoshi(pending),
                spendable: Satoshi(spendable),
            })
        })
    }
}

impl<N, U> TransferBuilder<Bitcoin> for BitcoinProductionWallet<N, U>
where
    N: Send + Sync,
    U: Send + Sync,
{
    fn build_transfer<'a>(
        &'a self,
        request: BitcoinBuildRequest,
    ) -> BoxFuture<'a, Result<UnsignedBitcoinTransaction, ChainError>> {
        Box::pin(async move { crate::BitcoinTransactionBuilder::build(&self.codec, request) })
    }
}

impl<N, U> TransactionSigner<Bitcoin> for BitcoinProductionWallet<N, U>
where
    N: Send + Sync,
    U: Send + Sync,
{
    fn sign_transaction<'a>(
        &'a self,
        transaction: UnsignedBitcoinTransaction,
        signer: &'a dyn Signer,
    ) -> BoxFuture<'a, Result<BitcoinSignedTransaction, ChainError>> {
        crate::BitcoinTransactionSigning::sign(&self.codec, transaction, signer)
    }
}

impl<N, U> Broadcaster<Bitcoin> for BitcoinProductionWallet<N, U>
where
    N: BitcoinNodeRpc,
    U: Send + Sync,
{
    fn broadcast<'a>(
        &'a self,
        transaction: BitcoinSignedTransaction,
    ) -> BoxFuture<'a, Result<BitcoinTransactionId, ChainError>> {
        Box::pin(async move {
            let preflight = self
                .node
                .preflight(&transaction, self.node_policy.maximum_fee_rate)
                .await
                .map_err(rpc_error)?;
            if !preflight.allowed {
                return Err(invalid_transaction(format!(
                    "Bitcoin transaction failed node preflight: {}",
                    preflight
                        .reject_reason
                        .as_deref()
                        .unwrap_or("unspecified policy rejection")
                )));
            }
            self.node
                .broadcast(transaction, self.node_policy.maximum_fee_rate)
                .await
                .map_err(rpc_error)
        })
    }
}

impl<N, U> TransactionReader<Bitcoin> for BitcoinProductionWallet<N, U>
where
    N: BitcoinNodeRpc,
    U: Send + Sync,
{
    fn transaction<'a>(
        &'a self,
        id: &'a BitcoinTransactionId,
    ) -> BoxFuture<'a, Result<Option<BitcoinReceipt>, ChainError>> {
        Box::pin(async move { self.node.receipt(id).await.map_err(rpc_error) })
    }
}

fn checked_sum(total: u64, value: u64) -> Result<u64, ChainError> {
    total
        .checked_add(value)
        .ok_or_else(|| invalid_transaction("Bitcoin balance overflowed the u64 satoshi range"))
}

fn rpc_error(error: indexing::SourceError) -> ChainError {
    ChainError {
        kind: ChainErrorKind::RpcUnavailable,
        message: format!("Bitcoin RPC operation failed: {error}"),
    }
}

fn invalid_transaction(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::InvalidTransaction,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{Address, CompressedPublicKey, PublicKey};
    use futures_executor::block_on;
    use indexing::{BlockHash, BlockHeight, BlockRef, SourceError};

    use super::*;

    struct ScriptedUtxos {
        values: Vec<crate::BitcoinRpcUtxo>,
    }

    struct ScriptedNode;

    impl BitcoinNodeRpc for ScriptedNode {
        fn canonical_hash<'a>(
            &'a self,
            _height: BlockHeight,
        ) -> BoxFuture<'a, Result<Option<BlockHash>, SourceError>> {
            Box::pin(async { Ok(Some(BlockHash(vec![9; 32]))) })
        }

        fn estimate_fee_rate<'a>(
            &'a self,
            _target_blocks: u16,
        ) -> BoxFuture<'a, Result<SatoshisPerKvb, SourceError>> {
            Box::pin(async { Err(unused_source()) })
        }

        fn preflight<'a>(
            &'a self,
            _transaction: &'a BitcoinSignedTransaction,
            _max_fee_rate: SatoshisPerKvb,
        ) -> BoxFuture<'a, Result<crate::BitcoinPreflight, SourceError>> {
            Box::pin(async { Err(unused_source()) })
        }

        fn broadcast<'a>(
            &'a self,
            _transaction: BitcoinSignedTransaction,
            _max_fee_rate: SatoshisPerKvb,
        ) -> BoxFuture<'a, Result<BitcoinTransactionId, SourceError>> {
            Box::pin(async { Err(unused_source()) })
        }

        fn receipt<'a>(
            &'a self,
            _id: &'a BitcoinTransactionId,
        ) -> BoxFuture<'a, Result<Option<BitcoinReceipt>, SourceError>> {
            Box::pin(async { Err(unused_source()) })
        }
    }

    fn unused_source() -> SourceError {
        SourceError {
            message: "unused test source operation".to_owned(),
            retryable: false,
        }
    }

    impl BitcoinUtxoSource for ScriptedUtxos {
        fn utxos<'a>(
            &'a self,
            _addresses: Vec<BitcoinAddress>,
        ) -> BoxFuture<'a, Result<crate::BitcoinUtxoSet, SourceError>> {
            let values = self.values.clone();
            Box::pin(async move {
                Ok(crate::BitcoinUtxoSet {
                    checkpoint: BlockRef {
                        height: BlockHeight(42),
                        hash: BlockHash(vec![9; 32]),
                        parent_hash: Some(BlockHash(vec![8; 32])),
                        timestamp: Some(1_000),
                    },
                    outputs: values,
                })
            })
        }
    }

    fn address() -> BitcoinAddress {
        let public_key = PublicKey::from_slice(&[
            0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test public key must parse");
        BitcoinAddress(
            Address::p2wpkh(
                &CompressedPublicKey::try_from(public_key)
                    .expect("test public key must be compressed"),
                bitcoin::Network::Regtest,
            )
            .to_string(),
        )
    }

    #[test]
    fn ix_utxo_script_must_match_the_requested_address() {
        let wallet = BitcoinProductionWallet::new(
            BitcoinNetwork::Regtest,
            ScriptedNode,
            ScriptedUtxos {
                values: vec![crate::BitcoinRpcUtxo {
                    transaction_id: [1; 32],
                    output_index: 0,
                    value: Satoshi(42),
                    script_pubkey: vec![0x51],
                    confirmations: 1,
                    coinbase: false,
                }],
            },
            BitcoinNodePolicy {
                fee_estimate_target_blocks: 6,
                maximum_fee_rate: SatoshisPerKvb::new(10_000),
            },
        )
        .expect("test policy must be valid");

        let error = block_on(BalanceReader::<Bitcoin>::balance(
            &wallet,
            &address(),
            &BitcoinAsset::Native,
        ))
        .expect_err("IX script mismatch must fail closed");

        assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
        assert!(error.message.contains("does not match"));
    }
}
