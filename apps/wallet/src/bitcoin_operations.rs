//! Stateless production Bitcoin operation adapter.
//!
//! PS supplies and reserves exact outpoints. This adapter re-reads those
//! outpoints from the active IX projection immediately before signing and has
//! no database, retry queue, reservation, or collection-workflow state.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use chain_bitcoin::{
    BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB, BitcoinAddress, BitcoinAddressGenerator,
    BitcoinBuildRequest, BitcoinCollectionAttribution, BitcoinCollectionRequirement,
    BitcoinNetwork, BitcoinNodeRpc, BitcoinOutput, BitcoinPreflight, BitcoinReceipt,
    BitcoinRpcUtxo, BitcoinSignedTransaction, BitcoinTransactionBuilder, BitcoinTransactionCodec,
    BitcoinTransactionId, BitcoinTransactionSigning, BitcoinUtxoSet, BitcoinUtxoSource, Satoshi,
    SatoshisPerKvb,
};
use chain_contract::{
    Balance, ChainError, ChainErrorKind, DepositAddressGenerator, GeneratedAddress,
};
use signer::{KeyProvisioner, Signer};

use crate::bitcoin_api::{
    BitcoinCollectionRequirementsRequest, BitcoinCollectionSignRequest, BitcoinExactInput,
    BitcoinPreparedCollection, BitcoinPreparedTransaction, BitcoinTransferSignRequest,
    BitcoinWalletOperations, OperationFuture,
};

const COINBASE_MATURITY: u64 = 100;

/// Deployment policy applied to every stateless Bitcoin request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitcoinOperationPolicy {
    pub minimum_confirmations: u64,
    pub fee_target_blocks: u16,
    pub maximum_fee_rate: SatoshisPerKvb,
    pub maximum_inputs: usize,
    pub maximum_outputs: usize,
}

impl BitcoinOperationPolicy {
    pub fn validate(self) -> Result<Self, ChainError> {
        if self.minimum_confirmations == 0 {
            return Err(invalid_transaction(
                "Bitcoin minimum confirmations must be greater than zero",
            ));
        }
        if self.fee_target_blocks == 0 {
            return Err(invalid_transaction(
                "Bitcoin fee target must be greater than zero",
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
        if self.maximum_inputs == 0 || self.maximum_outputs == 0 {
            return Err(invalid_transaction(
                "Bitcoin request count limits must be greater than zero",
            ));
        }
        Ok(self)
    }
}

/// Production-facing Bitcoin WS adapter with independently injected node,
/// active IX projection, key provisioning, and signing boundaries.
pub struct BitcoinOperations<N, U, K, S> {
    network: BitcoinNetwork,
    node: Arc<N>,
    utxos: Arc<U>,
    keys: K,
    signer: S,
    codec: BitcoinTransactionCodec,
    policy: BitcoinOperationPolicy,
}

impl<N, U, K, S> BitcoinOperations<N, U, K, S> {
    pub fn new(
        network: BitcoinNetwork,
        node: Arc<N>,
        utxos: Arc<U>,
        keys: K,
        signer: S,
        policy: BitcoinOperationPolicy,
    ) -> Result<Self, ChainError> {
        Ok(Self {
            network,
            node,
            utxos,
            keys,
            signer,
            codec: BitcoinTransactionCodec::new(network),
            policy: policy.validate()?,
        })
    }

    async fn effective_fee_rate(
        &self,
        requested: SatoshisPerKvb,
    ) -> Result<SatoshisPerKvb, ChainError>
    where
        N: BitcoinNodeRpc,
    {
        if requested.satoshis_per_kvb() == 0 || requested > self.policy.maximum_fee_rate {
            return Err(ChainError {
                kind: ChainErrorKind::FeeUnavailable,
                message: "requested Bitcoin fee rate is zero or exceeds the configured maximum"
                    .to_owned(),
            });
        }
        let estimated = self
            .node
            .estimate_fee_rate(self.policy.fee_target_blocks)
            .await
            .map_err(source_error)?;
        let effective = requested.max(estimated);
        if effective > self.policy.maximum_fee_rate {
            return Err(ChainError {
                kind: ChainErrorKind::FeeUnavailable,
                message: "Bitcoin Core fee estimate exceeds the configured maximum".to_owned(),
            });
        }
        Ok(effective)
    }

    async fn revalidate_inputs(
        &self,
        inputs: &[BitcoinExactInput],
    ) -> Result<Vec<chain_bitcoin::BitcoinUtxo>, ChainError>
    where
        N: BitcoinNodeRpc,
        U: BitcoinUtxoSource,
    {
        validate_unique_inputs(inputs)?;
        if inputs.len() > self.policy.maximum_inputs {
            return Err(invalid_transaction(
                "Bitcoin request exceeds the configured input limit",
            ));
        }

        let addresses = inputs
            .iter()
            .map(|input| input.address.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        // The IX route reads only the active projection generation. Its
        // generation-bound pagination rejects a cursor if activation changes
        // while this fresh lookup is in progress.
        let current = self.canonical_utxos(addresses).await?;
        let mut by_outpoint = BTreeMap::new();
        for output in current {
            let key = (output.transaction_id, output.output_index);
            if by_outpoint.insert(key, output).is_some() {
                return Err(rpc_unavailable(
                    "Bitcoin IX returned a duplicate active UTXO",
                ));
            }
        }

        inputs
            .iter()
            .map(|input| {
                let selected = by_outpoint
                    .get(&(input.transaction_id.0, input.output_index))
                    .ok_or_else(|| {
                        invalid_transaction(
                            "selected Bitcoin outpoint is spent, stale, or absent from the active IX projection",
                        )
                    })?;
                validate_current_output(input, selected, self.policy.minimum_confirmations)?;
                input.to_chain_utxo(self.network)
            })
            .collect()
    }

    async fn canonical_utxos(
        &self,
        addresses: Vec<BitcoinAddress>,
    ) -> Result<Vec<BitcoinRpcUtxo>, ChainError>
    where
        N: BitcoinNodeRpc,
        U: BitcoinUtxoSource,
    {
        let BitcoinUtxoSet {
            checkpoint,
            outputs,
        } = self.utxos.utxos(addresses).await.map_err(source_error)?;
        let canonical = self
            .node
            .canonical_hash(checkpoint.height)
            .await
            .map_err(source_error)?;
        if canonical.as_ref() != Some(&checkpoint.hash) {
            return Err(rpc_unavailable(
                "Bitcoin IX checkpoint does not match Bitcoin Core",
            ));
        }
        Ok(outputs)
    }

    fn validate_output_count(&self, count: usize) -> Result<(), ChainError> {
        if count == 0 || count > self.policy.maximum_outputs {
            return Err(invalid_transaction(
                "Bitcoin request has no outputs or exceeds the configured output limit",
            ));
        }
        Ok(())
    }
}

impl<N, U, K, S> BitcoinWalletOperations for BitcoinOperations<N, U, K, S>
where
    N: BitcoinNodeRpc + 'static,
    U: BitcoinUtxoSource + 'static,
    K: KeyProvisioner + Send + Sync + 'static,
    S: Signer + Send + Sync + 'static,
{
    fn generate_address<'a>(
        &'a self,
        request: chain_bitcoin::BitcoinGenerateAddress,
    ) -> OperationFuture<'a, Result<GeneratedAddress<BitcoinAddress>, ChainError>> {
        Box::pin(async move {
            if request.network != self.network {
                return Err(ChainError {
                    kind: ChainErrorKind::InvalidAddress,
                    message: "Bitcoin address request uses the wrong configured network".to_owned(),
                });
            }
            BitcoinAddressGenerator
                .generate_address(request, &self.keys)
                .await
        })
    }

    fn balance<'a>(
        &'a self,
        address: BitcoinAddress,
    ) -> OperationFuture<'a, Result<Balance<Satoshi>, ChainError>> {
        Box::pin(async move {
            let outputs = self.canonical_utxos(vec![address]).await?;
            let mut confirmed = 0_u64;
            let mut pending = 0_u64;
            let mut spendable = 0_u64;
            for output in outputs {
                if output.confirmations == 0 {
                    pending = checked_add(pending, output.value.0)?;
                } else {
                    confirmed = checked_add(confirmed, output.value.0)?;
                }
                if is_spendable(&output, self.policy.minimum_confirmations) {
                    spendable = checked_add(spendable, output.value.0)?;
                }
            }
            Ok(Balance {
                confirmed: Satoshi(confirmed),
                pending: Satoshi(pending),
                spendable: Satoshi(spendable),
            })
        })
    }

    fn sign_transfer<'a>(
        &'a self,
        request: BitcoinTransferSignRequest,
    ) -> OperationFuture<'a, Result<BitcoinPreparedTransaction, ChainError>> {
        Box::pin(async move {
            self.validate_output_count(request.recipients.len())?;
            let fee_rate = self.effective_fee_rate(request.fee_rate).await?;
            // Keep the canonical IX re-read as the final remote operation
            // before deterministic construction and custody signing.
            let available = self.revalidate_inputs(&request.inputs).await?;
            let unsigned = self.codec.build(BitcoinBuildRequest {
                signing_operation_id: request.signing_operation_id,
                available,
                recipients: request.recipients,
                change_address: request.change_address,
                fee_rate,
                drain_wallet: false,
            })?;
            if unsigned.inputs.len() != request.inputs.len() {
                return Err(invalid_transaction(
                    "exact Bitcoin selection contains an input that the transaction would not spend",
                ));
            }
            self.validate_output_count(unsigned.outputs.len())?;
            let outputs = unsigned.outputs.clone();
            let fee = transaction_fee(&unsigned)?;
            let transaction = self.codec.sign(unsigned, &self.signer).await?;
            let virtual_size = transaction.virtual_size()?;
            validate_fee_cap(fee, virtual_size, self.policy.maximum_fee_rate)?;
            Ok(BitcoinPreparedTransaction {
                transaction,
                inputs: request.inputs,
                outputs,
                fee,
                virtual_size,
            })
        })
    }

    fn collection_requirements<'a>(
        &'a self,
        request: BitcoinCollectionRequirementsRequest,
    ) -> OperationFuture<'a, Result<Vec<BitcoinCollectionRequirement>, ChainError>> {
        Box::pin(async move {
            if request.sources.is_empty() || request.sources.len() > self.policy.maximum_inputs {
                return Err(invalid_transaction(
                    "Bitcoin requirement query has no sources or exceeds the configured limit",
                ));
            }
            let outputs = self.canonical_utxos(request.sources.clone()).await?;
            let mut requirements = Vec::new();
            for address in request.sources {
                let script = address
                    .script_pubkey_for_network(self.network)?
                    .into_bytes();
                if !outputs.iter().any(|output| {
                    output.script_pubkey == script
                        && is_spendable(output, self.policy.minimum_confirmations)
                }) {
                    requirements.push(BitcoinCollectionRequirement::NoSpendableOutputs { address });
                }
            }
            Ok(requirements)
        })
    }

    fn sign_collection<'a>(
        &'a self,
        request: BitcoinCollectionSignRequest,
    ) -> OperationFuture<'a, Result<BitcoinPreparedCollection, ChainError>> {
        Box::pin(async move {
            self.validate_output_count(1)?;
            for source in &request.sources {
                if source
                    .inputs
                    .iter()
                    .any(|input| input.address != source.address || input.key != source.key)
                {
                    return Err(invalid_transaction(
                        "Bitcoin collection input ownership does not match its source",
                    ));
                }
            }
            let inputs = request
                .sources
                .iter()
                .flat_map(|source| source.inputs.iter().cloned())
                .collect::<Vec<_>>();
            let fee_rate = self.effective_fee_rate(request.fee_rate).await?;
            // Keep the canonical IX re-read as the final remote operation
            // before deterministic construction and custody signing.
            let available = self.revalidate_inputs(&inputs).await?;
            let unsigned = self.codec.build(BitcoinBuildRequest {
                signing_operation_id: request.signing_operation_id,
                available,
                recipients: vec![BitcoinOutput {
                    address: request.destination.clone(),
                    value: Satoshi(0),
                }],
                change_address: request.destination,
                fee_rate,
                drain_wallet: true,
            })?;
            let outputs = unsigned.outputs.clone();
            let fee = transaction_fee(&unsigned)?;
            let transaction = self.codec.sign(unsigned, &self.signer).await?;
            let virtual_size = transaction.virtual_size()?;
            validate_fee_cap(fee, virtual_size, self.policy.maximum_fee_rate)?;
            let attribution = request
                .sources
                .into_iter()
                .map(|source| {
                    let gross_input = source
                        .inputs
                        .iter()
                        .try_fold(0_u64, |total, input| checked_add(total, input.value.0))?;
                    Ok(BitcoinCollectionAttribution {
                        address: source.address,
                        key: source.key,
                        gross_input: Satoshi(gross_input),
                    })
                })
                .collect::<Result<Vec<_>, ChainError>>()?;
            Ok(BitcoinPreparedCollection {
                prepared: BitcoinPreparedTransaction {
                    transaction,
                    inputs,
                    outputs,
                    fee,
                    virtual_size,
                },
                attribution,
            })
        })
    }

    fn broadcast<'a>(
        &'a self,
        transaction: BitcoinSignedTransaction,
    ) -> OperationFuture<'a, Result<BitcoinTransactionId, ChainError>> {
        Box::pin(async move {
            let expected_id = transaction.id();
            let preflight = self
                .node
                .preflight(&transaction, self.policy.maximum_fee_rate)
                .await
                .map_err(source_error)?;
            validate_preflight(&transaction, preflight)?;
            let returned = self
                .node
                .broadcast(transaction, self.policy.maximum_fee_rate)
                .await
                .map_err(source_error)?;
            if returned != expected_id {
                return Err(rpc_unavailable(
                    "Bitcoin Core broadcast returned a different transaction ID",
                ));
            }
            Ok(returned)
        })
    }

    fn receipt<'a>(
        &'a self,
        transaction_id: BitcoinTransactionId,
    ) -> OperationFuture<'a, Result<Option<BitcoinReceipt>, ChainError>> {
        Box::pin(async move {
            self.node
                .receipt(&transaction_id)
                .await
                .map_err(source_error)
        })
    }
}

fn validate_current_output(
    requested: &BitcoinExactInput,
    current: &BitcoinRpcUtxo,
    minimum_confirmations: u64,
) -> Result<(), ChainError> {
    if requested.value.0 == 0 {
        return Err(invalid_transaction(
            "selected Bitcoin outpoint must have a non-zero value",
        ));
    }
    if current.value != requested.value || current.script_pubkey != requested.script_pubkey {
        return Err(invalid_transaction(
            "selected Bitcoin outpoint value or ownership script changed in IX",
        ));
    }
    if current.confirmations < minimum_confirmations {
        return Err(invalid_transaction(
            "selected Bitcoin outpoint does not meet the confirmation policy",
        ));
    }
    if current.coinbase && current.confirmations < COINBASE_MATURITY {
        return Err(invalid_transaction(
            "selected Bitcoin coinbase outpoint is not mature",
        ));
    }
    Ok(())
}

fn validate_unique_inputs(inputs: &[BitcoinExactInput]) -> Result<(), ChainError> {
    if inputs.is_empty() {
        return Err(invalid_transaction(
            "Bitcoin signing requires at least one exact input",
        ));
    }
    let mut seen = BTreeSet::new();
    if inputs
        .iter()
        .any(|input| !seen.insert((input.transaction_id, input.output_index)))
    {
        return Err(invalid_transaction(
            "Bitcoin exact input outpoints must be unique",
        ));
    }
    Ok(())
}

fn is_spendable(output: &BitcoinRpcUtxo, minimum_confirmations: u64) -> bool {
    output.value.0 > 0
        && output.confirmations >= minimum_confirmations
        && (!output.coinbase || output.confirmations >= COINBASE_MATURITY)
}

fn transaction_fee(
    transaction: &chain_bitcoin::UnsignedBitcoinTransaction,
) -> Result<Satoshi, ChainError> {
    let inputs = transaction
        .inputs
        .iter()
        .try_fold(0_u64, |total, input| checked_add(total, input.utxo.value.0))?;
    let outputs = transaction
        .outputs
        .iter()
        .try_fold(0_u64, |total, output| checked_add(total, output.value.0))?;
    inputs
        .checked_sub(outputs)
        .map(Satoshi)
        .ok_or_else(|| invalid_transaction("Bitcoin outputs exceed selected inputs"))
}

fn validate_preflight(
    transaction: &BitcoinSignedTransaction,
    preflight: BitcoinPreflight,
) -> Result<(), ChainError> {
    if !preflight.allowed {
        return Err(ChainError {
            kind: ChainErrorKind::Rejected,
            message: preflight
                .reject_reason
                .map(|reason| format!("Bitcoin Core preflight rejected transaction: {reason}"))
                .unwrap_or_else(|| "Bitcoin Core preflight rejected transaction".to_owned()),
        });
    }
    if let Some(returned) = preflight.virtual_size {
        if returned != transaction.virtual_size()? {
            return Err(rpc_unavailable(
                "Bitcoin Core preflight returned a different virtual size",
            ));
        }
    }
    Ok(())
}

fn validate_fee_cap(
    fee: Satoshi,
    virtual_size: u64,
    maximum_fee_rate: SatoshisPerKvb,
) -> Result<(), ChainError> {
    let maximum = u128::from(maximum_fee_rate.satoshis_per_kvb())
        .checked_mul(u128::from(virtual_size))
        .and_then(|value| value.checked_add(999))
        .map(|value| value / 1_000)
        .ok_or_else(|| invalid_transaction("Bitcoin maximum fee calculation overflowed"))?;
    if u128::from(fee.0) > maximum {
        return Err(ChainError {
            kind: ChainErrorKind::FeeUnavailable,
            message: "constructed Bitcoin transaction exceeds the configured maximum fee rate"
                .to_owned(),
        });
    }
    Ok(())
}

fn checked_add(left: u64, right: u64) -> Result<u64, ChainError> {
    left.checked_add(right)
        .ok_or_else(|| invalid_transaction("Bitcoin satoshi amount overflowed u64"))
}

fn source_error(error: indexing::SourceError) -> ChainError {
    rpc_unavailable(format!("Bitcoin dependency request failed: {error}"))
}

fn rpc_unavailable(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::RpcUnavailable,
        message: message.into(),
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
    use super::*;
    use chain_bitcoin::{
        BitcoinAddressKind, BitcoinGenerateAddress, BitcoinPreflight, BitcoinTransactionId,
    };
    use futures_executor::block_on;
    use indexing::{BlockHash, BlockHeight, BlockRef, SourceError};
    use signer::OperationId;
    use signer_local::LocalSigner;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct MockNode {
        allowed: AtomicBool,
        canonical_matches: AtomicBool,
        broadcasts: AtomicUsize,
    }

    impl MockNode {
        fn accepting() -> Self {
            Self {
                allowed: AtomicBool::new(true),
                canonical_matches: AtomicBool::new(true),
                broadcasts: AtomicUsize::new(0),
            }
        }
    }

    impl BitcoinNodeRpc for MockNode {
        fn canonical_hash<'a>(
            &'a self,
            _height: BlockHeight,
        ) -> chain_bitcoin::BoxFuture<'a, Result<Option<BlockHash>, SourceError>> {
            let matches = self.canonical_matches.load(Ordering::Relaxed);
            Box::pin(async move { Ok(Some(BlockHash(vec![if matches { 9 } else { 7 }; 32]))) })
        }

        fn estimate_fee_rate<'a>(
            &'a self,
            _target_blocks: u16,
        ) -> chain_bitcoin::BoxFuture<'a, Result<SatoshisPerKvb, SourceError>> {
            Box::pin(async { Ok(SatoshisPerKvb::new(1_000)) })
        }

        fn preflight<'a>(
            &'a self,
            _transaction: &'a BitcoinSignedTransaction,
            _maximum_fee_rate: SatoshisPerKvb,
        ) -> chain_bitcoin::BoxFuture<'a, Result<BitcoinPreflight, SourceError>> {
            let allowed = self.allowed.load(Ordering::Relaxed);
            Box::pin(async move {
                Ok(BitcoinPreflight {
                    allowed,
                    reject_reason: (!allowed).then(|| "deterministic rejection".to_owned()),
                    virtual_size: None,
                    base_fee: None,
                })
            })
        }

        fn broadcast<'a>(
            &'a self,
            transaction: BitcoinSignedTransaction,
            _maximum_fee_rate: SatoshisPerKvb,
        ) -> chain_bitcoin::BoxFuture<'a, Result<BitcoinTransactionId, SourceError>> {
            self.broadcasts.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move { Ok(transaction.id()) })
        }

        fn receipt<'a>(
            &'a self,
            _id: &'a BitcoinTransactionId,
        ) -> chain_bitcoin::BoxFuture<'a, Result<Option<BitcoinReceipt>, SourceError>> {
            Box::pin(async { Ok(None) })
        }
    }

    #[derive(Clone)]
    struct MockUtxos(Vec<BitcoinRpcUtxo>);

    impl BitcoinUtxoSource for MockUtxos {
        fn utxos<'a>(
            &'a self,
            _addresses: Vec<BitcoinAddress>,
        ) -> chain_bitcoin::BoxFuture<'a, Result<BitcoinUtxoSet, SourceError>> {
            let outputs = self.0.clone();
            Box::pin(async move {
                Ok(BitcoinUtxoSet {
                    checkpoint: BlockRef {
                        height: BlockHeight(42),
                        hash: BlockHash(vec![9; 32]),
                        parent_hash: Some(BlockHash(vec![8; 32])),
                        timestamp: Some(1_000),
                    },
                    outputs,
                })
            })
        }
    }

    struct Fixture {
        operations: BitcoinOperations<MockNode, MockUtxos, Arc<LocalSigner>, Arc<LocalSigner>>,
        node: Arc<MockNode>,
        input: BitcoinExactInput,
        recipient: BitcoinAddress,
    }

    fn operation(value: &str) -> OperationId {
        OperationId::new(value).expect("test operation ID must be valid")
    }

    fn fixture(mut current: impl FnMut(&BitcoinExactInput) -> Vec<BitcoinRpcUtxo>) -> Fixture {
        let custody = Arc::new(LocalSigner::ephemeral_for_testing());
        let source = block_on(BitcoinAddressGenerator.generate_address(
            BitcoinGenerateAddress::new(
                BitcoinNetwork::Regtest,
                BitcoinAddressKind::SegwitV0,
                operation("provision-test-source"),
                "test-source",
            ),
            custody.as_ref(),
        ))
        .expect("source address must be generated");
        let recipient = block_on(BitcoinAddressGenerator.generate_address(
            BitcoinGenerateAddress::new(
                BitcoinNetwork::Regtest,
                BitcoinAddressKind::Taproot,
                operation("provision-test-recipient"),
                "test-recipient",
            ),
            custody.as_ref(),
        ))
        .expect("recipient address must be generated")
        .address;
        let script = source
            .address
            .script_pubkey_for_network(BitcoinNetwork::Regtest)
            .expect("generated address script must materialize")
            .into_bytes();
        let input = BitcoinExactInput {
            transaction_id: BitcoinTransactionId([7; 32]),
            output_index: 2,
            value: Satoshi(100_000),
            script_pubkey: script,
            address: source.address,
            key: source.key,
        };
        let node = Arc::new(MockNode::accepting());
        let operations = BitcoinOperations::new(
            BitcoinNetwork::Regtest,
            Arc::clone(&node),
            Arc::new(MockUtxos(current(&input))),
            Arc::clone(&custody),
            custody,
            BitcoinOperationPolicy {
                minimum_confirmations: 6,
                fee_target_blocks: 6,
                maximum_fee_rate: SatoshisPerKvb::new(100_000),
                maximum_inputs: 10,
                maximum_outputs: 10,
            },
        )
        .expect("operation policy must be valid");
        Fixture {
            operations,
            node,
            input,
            recipient,
        }
    }

    fn current(input: &BitcoinExactInput) -> BitcoinRpcUtxo {
        BitcoinRpcUtxo {
            transaction_id: input.transaction_id.0,
            output_index: input.output_index,
            value: input.value,
            script_pubkey: input.script_pubkey.clone(),
            confirmations: 6,
            coinbase: false,
        }
    }

    fn transfer(fixture: &Fixture) -> BitcoinTransferSignRequest {
        BitcoinTransferSignRequest {
            signing_operation_id: operation("sign-test-transfer"),
            inputs: vec![fixture.input.clone()],
            recipients: vec![BitcoinOutput {
                address: fixture.recipient.clone(),
                value: Satoshi(20_000),
            }],
            change_address: fixture.input.address.clone(),
            fee_rate: SatoshisPerKvb::new(1_000),
        }
    }

    #[test]
    fn operation_policy_enforces_bitcoin_core_max_fee_rate_limit() {
        let policy = BitcoinOperationPolicy {
            minimum_confirmations: 1,
            fee_target_blocks: 1,
            maximum_fee_rate: SatoshisPerKvb::new(BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB),
            maximum_inputs: 1,
            maximum_outputs: 1,
        };
        policy.validate().expect("Core's exact maximum must pass");
        assert!(
            BitcoinOperationPolicy {
                maximum_fee_rate: SatoshisPerKvb::new(
                    BITCOIN_CORE_MAX_FEE_RATE_SATOSHIS_PER_KVB + 1,
                ),
                ..policy
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn signing_rejects_spent_stale_or_mismatched_outpoints() {
        let missing = fixture(|_| Vec::new());
        let error = block_on(missing.operations.sign_transfer(transfer(&missing)))
            .expect_err("missing active UTXO must be rejected");
        assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);

        let mismatched = fixture(|input| {
            let mut output = current(input);
            output.value = Satoshi(input.value.0 + 1);
            vec![output]
        });
        let error = block_on(mismatched.operations.sign_transfer(transfer(&mismatched)))
            .expect_err("changed value must be rejected");
        assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
    }

    #[test]
    fn ix_checkpoint_must_match_wallet_core_before_signing() {
        let fixture = fixture(|input| vec![current(input)]);
        fixture
            .node
            .canonical_matches
            .store(false, Ordering::Relaxed);

        let error = block_on(fixture.operations.sign_transfer(transfer(&fixture)))
            .expect_err("a different Core canonical hash must fail closed");
        assert_eq!(error.kind, ChainErrorKind::RpcUnavailable);
        assert!(error.message.contains("checkpoint"));
    }

    #[test]
    fn signing_rejects_unconfirmed_and_immature_coinbase_outpoints() {
        let unconfirmed = fixture(|input| {
            let mut output = current(input);
            output.confirmations = 5;
            vec![output]
        });
        assert!(block_on(unconfirmed.operations.sign_transfer(transfer(&unconfirmed))).is_err());

        let immature = fixture(|input| {
            let mut output = current(input);
            output.confirmations = 99;
            output.coinbase = true;
            vec![output]
        });
        assert!(block_on(immature.operations.sign_transfer(transfer(&immature))).is_err());
    }

    #[test]
    fn zero_value_projection_outputs_are_factual_but_not_spendable() {
        let fixture = fixture(|input| {
            let mut output = current(input);
            output.value = Satoshi(0);
            vec![output]
        });
        let requirements = block_on(fixture.operations.collection_requirements(
            BitcoinCollectionRequirementsRequest {
                sources: vec![fixture.input.address.clone()],
            },
        ))
        .expect("zero output must not break the factual IX read");

        assert_eq!(
            requirements,
            vec![BitcoinCollectionRequirement::NoSpendableOutputs {
                address: fixture.input.address,
            }]
        );
    }

    #[test]
    fn exact_fresh_selection_signs_and_reports_review_data() {
        let fixture = fixture(|input| vec![current(input)]);
        let prepared = block_on(fixture.operations.sign_transfer(transfer(&fixture)))
            .expect("fresh exact selection must sign");

        assert_eq!(prepared.inputs, vec![fixture.input]);
        assert!(prepared.fee.0 > 0);
        assert!(prepared.virtual_size > 0);
        assert!(!prepared.transaction.consensus_bytes().is_empty());
    }

    #[test]
    fn duplicate_outpoints_are_rejected_before_signing() {
        let fixture = fixture(|input| vec![current(input)]);
        let mut request = transfer(&fixture);
        request.inputs.push(request.inputs[0].clone());
        let error = block_on(fixture.operations.sign_transfer(request))
            .expect_err("duplicate outpoint must fail");
        assert_eq!(error.kind, ChainErrorKind::InvalidTransaction);
    }

    #[test]
    fn preflight_rejection_prevents_broadcast() {
        let fixture = fixture(|input| vec![current(input)]);
        let prepared = block_on(fixture.operations.sign_transfer(transfer(&fixture)))
            .expect("transaction must sign before preflight");
        fixture.node.allowed.store(false, Ordering::Relaxed);

        let error = block_on(fixture.operations.broadcast(prepared.transaction))
            .expect_err("Core rejection must fail broadcast");
        assert_eq!(error.kind, ChainErrorKind::Rejected);
        assert_eq!(fixture.node.broadcasts.load(Ordering::Relaxed), 0);
    }
}
