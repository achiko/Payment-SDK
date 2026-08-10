use crate::{
    Bitcoin, BitcoinAddress, BitcoinAsset, BitcoinBatchCollectionRequest,
    BitcoinCollectionAttribution, BitcoinCollectionRequirement, BitcoinRpc, BitcoinRpcUtxo,
    BitcoinSignedTransaction, BitcoinTransactionCodec, BitcoinTransactionId, BitcoinUtxo, Satoshi,
    UnsignedBitcoinTransaction,
};
use bitcoin::{
    Address, CompressedPublicKey, Network, ScriptBuf, XOnlyPublicKey, address::NetworkUnchecked,
    secp256k1::Secp256k1,
};
use chain_contract::{
    Balance, BalanceReader, BoxFuture, Broadcaster, ChainError, ChainErrorKind,
    CollectionSubmission, Collector, DepositAddressGenerator, GeneratedAddress, TransactionReader,
    TransactionSigner, TransferBuilder, WalletAdapter, WalletFactory,
};
use indexing::SourceError;
use signer::{
    Curve, KeyProvisionRequest, KeyProvisioner, OperationId, PublicKey, PublicKeyFormat, Signer,
    SignerError,
};

const COINBASE_MATURITY: u64 = 100;
const P2WPKH_SATISFACTION_WEIGHT: u64 = 109;
const P2TR_SATISFACTION_WEIGHT: u64 = 67;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitcoinNetwork {
    Mainnet,
    Testnet3,
    Testnet4,
    Signet,
    Regtest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitcoinAddressKind {
    SegwitV0,
    /// The returned public key is the untweaked internal key. Address derivation
    /// applies the BIP341 TapTweak before encoding the output key.
    Taproot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitcoinGenerateAddress {
    pub network: BitcoinNetwork,
    pub kind: BitcoinAddressKind,
    pub key: KeyProvisionRequest,
}

impl BitcoinGenerateAddress {
    #[must_use]
    pub fn new(
        network: BitcoinNetwork,
        kind: BitcoinAddressKind,
        operation_id: OperationId,
        purpose: impl Into<String>,
    ) -> Self {
        Self {
            network,
            kind,
            key: KeyProvisionRequest {
                operation_id,
                curve: Curve::Secp256k1,
                public_key_format: required_public_key_format(kind),
                purpose: purpose.into(),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BitcoinAddressGenerator;

impl DepositAddressGenerator<Bitcoin> for BitcoinAddressGenerator {
    fn generate_address<'a>(
        &'a self,
        request: BitcoinGenerateAddress,
        keys: &'a dyn KeyProvisioner,
    ) -> BoxFuture<'a, Result<GeneratedAddress<BitcoinAddress>, ChainError>> {
        Box::pin(async move {
            let provisioned = keys
                .provision(request.key)
                .await
                .map_err(key_provision_error)?;
            validate_public_key(&provisioned.public_key, request.kind)?;

            let network = native_network(request.network);
            let address = match request.kind {
                BitcoinAddressKind::SegwitV0 => {
                    let public_key = CompressedPublicKey::from_slice(&provisioned.public_key.bytes)
                        .map_err(|error| {
                            invalid_public_key(format!(
                                "invalid compressed Bitcoin public key: {error}"
                            ))
                        })?;
                    Address::p2wpkh(&public_key, network)
                }
                BitcoinAddressKind::Taproot => {
                    let public_key = XOnlyPublicKey::from_slice(&provisioned.public_key.bytes)
                        .map_err(|error| {
                            invalid_public_key(format!(
                                "invalid x-only Bitcoin public key: {error}"
                            ))
                        })?;
                    let secp = Secp256k1::verification_only();
                    Address::p2tr(&secp, public_key, None, network)
                }
            };

            Ok(GeneratedAddress {
                address: BitcoinAddress(address.to_string()),
                key: provisioned.locator,
                public_key: provisioned.public_key,
            })
        })
    }
}

/// Complete stateless Bitcoin wallet adapter for native SegWit v0 and Taproot
/// key-path inputs.
#[derive(Debug)]
pub struct BitcoinWallet<R> {
    network: BitcoinNetwork,
    rpc: R,
    codec: BitcoinTransactionCodec,
}

impl<R> BitcoinWallet<R> {
    #[must_use]
    pub const fn new(network: BitcoinNetwork, rpc: R) -> Self {
        Self {
            network,
            rpc,
            codec: BitcoinTransactionCodec::new(network),
        }
    }

    #[must_use]
    pub const fn network(&self) -> BitcoinNetwork {
        self.network
    }

    #[must_use]
    pub const fn rpc(&self) -> &R {
        &self.rpc
    }
}

impl<R: BitcoinRpc> DepositAddressGenerator<Bitcoin> for BitcoinWallet<R> {
    fn generate_address<'a>(
        &'a self,
        request: BitcoinGenerateAddress,
        keys: &'a dyn KeyProvisioner,
    ) -> BoxFuture<'a, Result<GeneratedAddress<BitcoinAddress>, ChainError>> {
        Box::pin(async move {
            if request.network != self.network {
                return Err(ChainError {
                    kind: ChainErrorKind::InvalidAddress,
                    message: format!(
                        "Bitcoin address request uses {:?}, expected {:?}",
                        request.network, self.network
                    ),
                });
            }
            BitcoinAddressGenerator
                .generate_address(request, keys)
                .await
        })
    }
}

impl<R: BitcoinRpc> BalanceReader<Bitcoin> for BitcoinWallet<R> {
    fn balance<'a>(
        &'a self,
        address: &'a BitcoinAddress,
        _asset: &'a BitcoinAsset,
    ) -> BoxFuture<'a, Result<Balance<Satoshi>, ChainError>> {
        Box::pin(async move {
            let script = checked_script(self.network, address)?;
            let utxos = self
                .rpc
                .utxos(vec![script.as_bytes().to_vec()])
                .await
                .map_err(rpc_error)?;
            let mut confirmed = 0_u64;
            let mut pending = 0_u64;
            let mut spendable = 0_u64;
            for utxo in utxos
                .iter()
                .filter(|utxo| utxo.script_pubkey == script.as_bytes())
            {
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

impl<R: BitcoinRpc> TransferBuilder<Bitcoin> for BitcoinWallet<R> {
    fn build_transfer<'a>(
        &'a self,
        request: crate::BitcoinBuildRequest,
    ) -> BoxFuture<'a, Result<UnsignedBitcoinTransaction, ChainError>> {
        Box::pin(async move { crate::BitcoinTransactionBuilder::build(&self.codec, request) })
    }
}

impl<R: BitcoinRpc> TransactionSigner<Bitcoin> for BitcoinWallet<R> {
    fn sign_transaction<'a>(
        &'a self,
        transaction: UnsignedBitcoinTransaction,
        signer: &'a dyn Signer,
    ) -> BoxFuture<'a, Result<BitcoinSignedTransaction, ChainError>> {
        crate::BitcoinTransactionSigning::sign(&self.codec, transaction, signer)
    }
}

impl<R: BitcoinRpc> Broadcaster<Bitcoin> for BitcoinWallet<R> {
    fn broadcast<'a>(
        &'a self,
        transaction: BitcoinSignedTransaction,
    ) -> BoxFuture<'a, Result<BitcoinTransactionId, ChainError>> {
        Box::pin(async move { self.rpc.broadcast(transaction).await.map_err(rpc_error) })
    }
}

impl<R: BitcoinRpc> TransactionReader<Bitcoin> for BitcoinWallet<R> {
    fn transaction<'a>(
        &'a self,
        id: &'a BitcoinTransactionId,
    ) -> BoxFuture<'a, Result<Option<crate::BitcoinReceipt>, ChainError>> {
        Box::pin(async move { self.rpc.receipt(id).await.map_err(rpc_error) })
    }
}

impl<R: BitcoinRpc> Collector<Bitcoin> for BitcoinWallet<R> {
    fn requirements<'a>(
        &'a self,
        request: &'a BitcoinBatchCollectionRequest,
    ) -> BoxFuture<'a, Result<Vec<BitcoinCollectionRequirement>, ChainError>> {
        Box::pin(async move {
            let mut requirements = Vec::new();
            for source in &request.sources {
                let script = checked_script(self.network, &source.address)?;
                let utxos = self
                    .rpc
                    .utxos(vec![script.as_bytes().to_vec()])
                    .await
                    .map_err(rpc_error)?;
                if !utxos.iter().any(|utxo| {
                    utxo.script_pubkey == script.as_bytes()
                        && is_spendable(utxo, request.minimum_confirmations)
                }) {
                    requirements.push(BitcoinCollectionRequirement::NoSpendableOutputs {
                        address: source.address.clone(),
                    });
                }
            }
            Ok(requirements)
        })
    }

    fn collect<'a>(
        &'a self,
        request: BitcoinBatchCollectionRequest,
        signer: &'a dyn Signer,
    ) -> BoxFuture<
        'a,
        Result<
            CollectionSubmission<BitcoinTransactionId, BitcoinCollectionAttribution>,
            ChainError,
        >,
    > {
        Box::pin(async move {
            if request.sources.is_empty() {
                return Err(invalid_transaction(
                    "Bitcoin collection requires at least one source",
                ));
            }
            let mut available = Vec::new();
            let mut attribution = Vec::new();
            for source in &request.sources {
                let script = checked_script(self.network, &source.address)?;
                let utxos = self
                    .rpc
                    .utxos(vec![script.as_bytes().to_vec()])
                    .await
                    .map_err(rpc_error)?;
                let mut gross_input = 0_u64;
                for utxo in utxos.into_iter().filter(|utxo| {
                    utxo.script_pubkey == script.as_bytes()
                        && is_spendable(utxo, request.minimum_confirmations)
                }) {
                    gross_input = checked_sum(gross_input, utxo.value.0)?;
                    available.push(wallet_utxo(utxo, source.key.clone())?);
                }
                if gross_input > 0 {
                    attribution.push(BitcoinCollectionAttribution {
                        address: source.address.clone(),
                        key: source.key.clone(),
                        gross_input: Satoshi(gross_input),
                    });
                }
            }
            if available.is_empty() {
                return Err(ChainError {
                    kind: ChainErrorKind::InsufficientFunds,
                    message: "Bitcoin collection has no spendable UTXOs".to_owned(),
                });
            }
            let fee_rate = match request.fee_rate {
                Some(fee_rate) => fee_rate,
                None => self.rpc.estimate_fee_rate().await.map_err(rpc_error)?,
            };
            let unsigned = crate::BitcoinTransactionBuilder::build(
                &self.codec,
                crate::BitcoinBuildRequest {
                    signing_operation_id: request.signing_operation_id,
                    available,
                    recipients: vec![crate::BitcoinOutput {
                        address: request.destination.clone(),
                        value: Satoshi(0),
                    }],
                    change_address: request.destination,
                    fee_rate,
                    drain_wallet: true,
                },
            )?;
            let signed =
                crate::BitcoinTransactionSigning::sign(&self.codec, unsigned, signer).await?;
            let transaction_id = self.rpc.broadcast(signed).await.map_err(rpc_error)?;
            Ok(CollectionSubmission {
                transaction_id,
                attribution,
            })
        })
    }
}

impl<R: BitcoinRpc> WalletFactory<Bitcoin> for BitcoinWallet<R> {
    fn wallet_for<'a>(
        &'a self,
        _asset: &'a BitcoinAsset,
    ) -> Result<&'a dyn WalletAdapter<Bitcoin>, ChainError> {
        Ok(self)
    }
}

fn wallet_utxo(utxo: BitcoinRpcUtxo, key: signer::KeyLocator) -> Result<BitcoinUtxo, ChainError> {
    let script = ScriptBuf::from_bytes(utxo.script_pubkey.clone());
    let satisfaction_weight = if script.is_p2wpkh() {
        P2WPKH_SATISFACTION_WEIGHT
    } else if script.is_p2tr() {
        P2TR_SATISFACTION_WEIGHT
    } else {
        return Err(invalid_transaction(
            "Bitcoin collection supports P2WPKH and P2TR inputs only",
        ));
    };
    Ok(BitcoinUtxo {
        transaction_id: utxo.transaction_id,
        output_index: utxo.output_index,
        value: utxo.value,
        script_pubkey: utxo.script_pubkey,
        satisfaction_weight,
        key,
    })
}

fn is_spendable(utxo: &BitcoinRpcUtxo, minimum_confirmations: u64) -> bool {
    utxo.confirmations >= minimum_confirmations
        && (!utxo.coinbase || utxo.confirmations >= COINBASE_MATURITY)
}

fn checked_script(
    network: BitcoinNetwork,
    address: &BitcoinAddress,
) -> Result<ScriptBuf, ChainError> {
    address
        .0
        .parse::<Address<NetworkUnchecked>>()
        .map_err(|error| ChainError {
            kind: ChainErrorKind::InvalidAddress,
            message: format!("invalid Bitcoin address: {error}"),
        })?
        .require_network(native_network(network))
        .map(|address| address.script_pubkey())
        .map_err(|error| ChainError {
            kind: ChainErrorKind::InvalidAddress,
            message: format!("Bitcoin address is for the wrong network: {error}"),
        })
}

fn checked_sum(total: u64, value: u64) -> Result<u64, ChainError> {
    total
        .checked_add(value)
        .ok_or_else(|| invalid_transaction("Bitcoin balance overflowed the u64 satoshi range"))
}

fn rpc_error(error: SourceError) -> ChainError {
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

const fn required_public_key_format(kind: BitcoinAddressKind) -> PublicKeyFormat {
    match kind {
        BitcoinAddressKind::SegwitV0 => PublicKeyFormat::Compressed,
        BitcoinAddressKind::Taproot => PublicKeyFormat::XOnly,
    }
}

fn validate_public_key(public_key: &PublicKey, kind: BitcoinAddressKind) -> Result<(), ChainError> {
    let expected_format = required_public_key_format(kind);
    let expected_length = match kind {
        BitcoinAddressKind::SegwitV0 => 33,
        BitcoinAddressKind::Taproot => 32,
    };

    if public_key.curve != Curve::Secp256k1
        || public_key.format != expected_format
        || public_key.bytes.len() != expected_length
    {
        return Err(invalid_public_key(format!(
            "Bitcoin {kind:?} requires a secp256k1 {expected_format:?} public key with {expected_length} bytes"
        )));
    }

    Ok(())
}

const fn native_network(network: BitcoinNetwork) -> Network {
    match network {
        BitcoinNetwork::Mainnet => Network::Bitcoin,
        BitcoinNetwork::Testnet3 => Network::Testnet,
        BitcoinNetwork::Testnet4 => Network::Testnet4,
        BitcoinNetwork::Signet => Network::Signet,
        BitcoinNetwork::Regtest => Network::Regtest,
    }
}

fn key_provision_error(error: SignerError) -> ChainError {
    ChainError {
        kind: ChainErrorKind::Signer,
        message: format!("key provisioning failed: {error}"),
    }
}

fn invalid_public_key(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::InvalidAddress,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_executor::block_on;
    use indexing::{BlockHeight, BlockRef};
    use signer_local::LocalSigner;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct MockBitcoinRpc {
        utxos: Vec<BitcoinRpcUtxo>,
        broadcasts: AtomicUsize,
    }

    impl BitcoinRpc for MockBitcoinRpc {
        fn tip<'a>(&'a self) -> crate::BoxFuture<'a, Result<BlockRef, SourceError>> {
            Box::pin(async { Err(unused_rpc_error()) })
        }

        fn block_at<'a>(
            &'a self,
            _height: BlockHeight,
        ) -> crate::BoxFuture<'a, Result<crate::BitcoinBlock, SourceError>> {
            Box::pin(async { Err(unused_rpc_error()) })
        }

        fn utxos<'a>(
            &'a self,
            scripts: Vec<Vec<u8>>,
        ) -> crate::BoxFuture<'a, Result<Vec<BitcoinRpcUtxo>, SourceError>> {
            let utxos = self
                .utxos
                .iter()
                .filter(|utxo| scripts.contains(&utxo.script_pubkey))
                .cloned()
                .collect();
            Box::pin(async move { Ok(utxos) })
        }

        fn estimate_fee_rate<'a>(
            &'a self,
        ) -> crate::BoxFuture<'a, Result<crate::SatoshisPerKvb, SourceError>> {
            Box::pin(async { Ok(crate::SatoshisPerKvb::new(1_000)) })
        }

        fn broadcast<'a>(
            &'a self,
            transaction: BitcoinSignedTransaction,
        ) -> crate::BoxFuture<'a, Result<BitcoinTransactionId, SourceError>> {
            self.broadcasts.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move { Ok(transaction.id()) })
        }

        fn receipt<'a>(
            &'a self,
            _id: &'a BitcoinTransactionId,
        ) -> crate::BoxFuture<'a, Result<Option<crate::BitcoinReceipt>, SourceError>> {
            Box::pin(async { Ok(None) })
        }
    }

    fn unused_rpc_error() -> SourceError {
        SourceError {
            message: "not used by wallet test".to_owned(),
            retryable: false,
        }
    }

    fn operation(value: impl Into<String>) -> OperationId {
        OperationId::new(value).expect("test operation ID must be valid")
    }

    #[test]
    fn generates_native_segwit_addresses_for_each_network() {
        let keys = LocalSigner::ephemeral_for_testing();
        let generator = BitcoinAddressGenerator;

        for (network, prefix) in [
            (BitcoinNetwork::Mainnet, "bc1q"),
            (BitcoinNetwork::Testnet3, "tb1q"),
            (BitcoinNetwork::Testnet4, "tb1q"),
            (BitcoinNetwork::Signet, "tb1q"),
            (BitcoinNetwork::Regtest, "bcrt1q"),
        ] {
            let generated = block_on(generator.generate_address(
                BitcoinGenerateAddress::new(
                    network,
                    BitcoinAddressKind::SegwitV0,
                    operation(format!("provision-segwit-{network:?}")),
                    "segwit-test",
                ),
                &keys,
            ))
            .expect("native SegWit address should be generated");

            assert!(generated.address.0.starts_with(prefix));
            assert_eq!(generated.public_key.curve, Curve::Secp256k1);
            assert_eq!(generated.public_key.format, PublicKeyFormat::Compressed);
            assert_eq!(generated.public_key.bytes.len(), 33);
        }
    }

    #[test]
    fn generates_taproot_addresses_for_each_network() {
        let keys = LocalSigner::ephemeral_for_testing();
        let generator = BitcoinAddressGenerator;

        for (network, prefix) in [
            (BitcoinNetwork::Mainnet, "bc1p"),
            (BitcoinNetwork::Testnet3, "tb1p"),
            (BitcoinNetwork::Testnet4, "tb1p"),
            (BitcoinNetwork::Signet, "tb1p"),
            (BitcoinNetwork::Regtest, "bcrt1p"),
        ] {
            let generated = block_on(generator.generate_address(
                BitcoinGenerateAddress::new(
                    network,
                    BitcoinAddressKind::Taproot,
                    operation(format!("provision-taproot-{network:?}")),
                    "taproot-test",
                ),
                &keys,
            ))
            .expect("Taproot address should be generated");

            assert!(generated.address.0.starts_with(prefix));
            assert_eq!(generated.public_key.curve, Curve::Secp256k1);
            assert_eq!(generated.public_key.format, PublicKeyFormat::XOnly);
            assert_eq!(generated.public_key.bytes.len(), 32);
        }
    }

    #[test]
    fn reads_confirmed_pending_and_spendable_bitcoin_balances() {
        let keys = LocalSigner::ephemeral_for_testing();
        let generated = block_on(BitcoinAddressGenerator.generate_address(
            BitcoinGenerateAddress::new(
                BitcoinNetwork::Regtest,
                BitcoinAddressKind::SegwitV0,
                operation("provision-balance-source"),
                "balance-source",
            ),
            &keys,
        ))
        .expect("balance source should be generated");
        let script = checked_script(BitcoinNetwork::Regtest, &generated.address)
            .expect("generated address should parse")
            .into_bytes();
        let wallet = BitcoinWallet::new(
            BitcoinNetwork::Regtest,
            MockBitcoinRpc {
                utxos: vec![
                    BitcoinRpcUtxo {
                        transaction_id: [1; 32],
                        output_index: 0,
                        value: Satoshi(5_000),
                        script_pubkey: script.clone(),
                        confirmations: 6,
                        coinbase: false,
                    },
                    BitcoinRpcUtxo {
                        transaction_id: [2; 32],
                        output_index: 0,
                        value: Satoshi(2_000),
                        script_pubkey: script.clone(),
                        confirmations: 0,
                        coinbase: false,
                    },
                    BitcoinRpcUtxo {
                        transaction_id: [3; 32],
                        output_index: 0,
                        value: Satoshi(7_000),
                        script_pubkey: script,
                        confirmations: 50,
                        coinbase: true,
                    },
                ],
                broadcasts: AtomicUsize::new(0),
            },
        );

        let balance = block_on(wallet.balance(&generated.address, &BitcoinAsset::Native))
            .expect("Bitcoin balance should be read");

        assert_eq!(balance.confirmed, Satoshi(12_000));
        assert_eq!(balance.pending, Satoshi(2_000));
        assert_eq!(balance.spendable, Satoshi(5_000));
    }

    #[test]
    fn batches_and_collects_spendable_bitcoin_utxos() {
        let keys = LocalSigner::ephemeral_for_testing();
        let source = block_on(BitcoinAddressGenerator.generate_address(
            BitcoinGenerateAddress::new(
                BitcoinNetwork::Regtest,
                BitcoinAddressKind::SegwitV0,
                operation("provision-collection-source"),
                "collection-source",
            ),
            &keys,
        ))
        .expect("collection source should be generated");
        let destination = block_on(BitcoinAddressGenerator.generate_address(
            BitcoinGenerateAddress::new(
                BitcoinNetwork::Regtest,
                BitcoinAddressKind::SegwitV0,
                operation("provision-collection-destination"),
                "collection-destination",
            ),
            &keys,
        ))
        .expect("collection destination should be generated");
        let script = checked_script(BitcoinNetwork::Regtest, &source.address)
            .expect("generated source should parse")
            .into_bytes();
        let wallet = BitcoinWallet::new(
            BitcoinNetwork::Regtest,
            MockBitcoinRpc {
                utxos: vec![BitcoinRpcUtxo {
                    transaction_id: [9; 32],
                    output_index: 1,
                    value: Satoshi(50_000),
                    script_pubkey: script,
                    confirmations: 6,
                    coinbase: false,
                }],
                broadcasts: AtomicUsize::new(0),
            },
        );
        let source_address = source.address.clone();
        let source_key = source.key.clone();
        let submission = block_on(wallet.collect(
            BitcoinBatchCollectionRequest {
                signing_operation_id: operation("sign-bitcoin-collection"),
                sources: vec![crate::BitcoinCollectionSource {
                    address: source.address,
                    key: source.key,
                    birthday: BlockHeight(0),
                }],
                destination: destination.address,
                minimum_confirmations: 1,
                fee_rate: None,
            },
            &keys,
        ))
        .expect("Bitcoin collection should succeed");

        assert_eq!(wallet.rpc().broadcasts.load(Ordering::Relaxed), 1);
        assert_eq!(
            submission.attribution,
            vec![BitcoinCollectionAttribution {
                address: source_address,
                key: source_key,
                gross_input: Satoshi(50_000),
            }]
        );
    }
}
