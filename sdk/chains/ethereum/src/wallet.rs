use crate::{
    Ethereum, EthereumAddress, EthereumAsset, EthereumCollectionAttribution,
    EthereumCollectionRequest, EthereumCollectionRequirement, EthereumPreparedCollection,
    EthereumRpc, EthereumSignedTransaction, EthereumTransactionCodec, EthereumTransactionId,
    EthereumTransferRequest, UnsignedEthereumTransaction, Wei,
};
use alloy_primitives::Address;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumGenerateAddress {
    /// Caller-selected network context. EOA key and address derivation itself is
    /// identical across Ethereum chain IDs.
    pub chain_id: u64,
    pub key: KeyProvisionRequest,
}

impl EthereumGenerateAddress {
    #[must_use]
    pub fn new(chain_id: u64, operation_id: OperationId, purpose: impl Into<String>) -> Self {
        Self {
            chain_id,
            key: KeyProvisionRequest {
                operation_id,
                curve: Curve::Secp256k1,
                public_key_format: PublicKeyFormat::Raw,
                purpose: purpose.into(),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EthereumAddressGenerator;

impl DepositAddressGenerator<Ethereum> for EthereumAddressGenerator {
    fn generate_address<'a>(
        &'a self,
        request: EthereumGenerateAddress,
        keys: &'a dyn KeyProvisioner,
    ) -> BoxFuture<'a, Result<GeneratedAddress<EthereumAddress>, ChainError>> {
        Box::pin(async move {
            let provisioned = keys
                .provision(request.key)
                .await
                .map_err(key_provision_error)?;
            let raw_public_key = raw_ethereum_public_key(&provisioned.public_key)?;
            let address = Address::from_raw_public_key(raw_public_key);

            Ok(GeneratedAddress {
                address: EthereumAddress(address.into_array()),
                key: provisioned.locator,
                public_key: provisioned.public_key,
            })
        })
    }
}

/// Complete stateless Ethereum wallet adapter. RPC state and custody are
/// injected; this value stores only immutable chain configuration.
#[derive(Debug)]
pub struct EthereumWallet<R> {
    chain_id: u64,
    rpc: R,
    codec: EthereumTransactionCodec,
}

impl<R> EthereumWallet<R> {
    #[must_use]
    pub const fn new(chain_id: u64, rpc: R) -> Self {
        Self {
            chain_id,
            rpc,
            codec: EthereumTransactionCodec,
        }
    }

    #[must_use]
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    #[must_use]
    pub const fn rpc(&self) -> &R {
        &self.rpc
    }

    fn validate_context(&self, context: &crate::EthereumBuildContext) -> Result<(), ChainError> {
        if context.chain_id != self.chain_id {
            return Err(invalid_transaction(format!(
                "Ethereum RPC returned chain ID {}, expected {}",
                context.chain_id, self.chain_id
            )));
        }
        Ok(())
    }
}

impl<R: EthereumRpc> EthereumWallet<R> {
    /// Builds and signs one collection transaction without broadcasting it.
    ///
    /// This separation lets the Payment Service durably persist the exact
    /// opaque envelope and transaction ID before the external broadcast side
    /// effect. The wallet retains no state between this and a later call to
    /// [`Broadcaster::broadcast`].
    pub async fn prepare_collection(
        &self,
        request: EthereumCollectionRequest,
        signer: &dyn Signer,
    ) -> Result<EthereumPreparedCollection, ChainError> {
        let (transfer, context, attribution) = self.collection_transfer(request).await?;
        let unsigned = crate::EthereumTransactionBuilder::build(&self.codec, transfer, context)?;
        let transaction =
            crate::EthereumTransactionSigning::sign(&self.codec, unsigned, signer).await?;
        Ok(EthereumPreparedCollection {
            transaction,
            attribution: vec![attribution],
        })
    }

    async fn collection_transfer(
        &self,
        request: EthereumCollectionRequest,
    ) -> Result<
        (
            EthereumTransferRequest,
            crate::EthereumBuildContext,
            EthereumCollectionAttribution,
        ),
        ChainError,
    > {
        match request {
            EthereumCollectionRequest::Native {
                signing_operation_id,
                from,
                key,
                destination,
            } => {
                let current = self
                    .rpc
                    .balance(from.clone(), &EthereumAsset::Native, None)
                    .await
                    .map_err(rpc_error)?;
                let mut transfer = EthereumTransferRequest::native(
                    signing_operation_id,
                    key,
                    from.clone(),
                    destination,
                    Wei::ZERO,
                );
                let context = self.rpc.build_context(&transfer).await.map_err(rpc_error)?;
                self.validate_context(&context)?;
                let maximum_fee = context
                    .max_fee_per_gas
                    .checked_mul_u64(context.gas_limit)
                    .ok_or_else(|| {
                        invalid_transaction("Ethereum maximum gas fee overflowed U256")
                    })?;
                let value = current
                    .checked_sub(&maximum_fee)
                    .ok_or_else(|| ChainError {
                        kind: ChainErrorKind::InsufficientFunds,
                        message: "Ethereum native balance cannot cover the maximum gas fee"
                            .to_owned(),
                    })?;
                if value.is_zero() {
                    return Err(ChainError {
                        kind: ChainErrorKind::InsufficientFunds,
                        message: "Ethereum native collection would transfer zero wei".to_owned(),
                    });
                }
                transfer.value = value.clone();
                Ok((
                    transfer,
                    context,
                    EthereumCollectionAttribution {
                        address: from,
                        asset: EthereumAsset::Native,
                        gross_debit: value,
                    },
                ))
            }
            EthereumCollectionRequest::Token {
                signing_operation_id,
                token,
                from,
                key,
                destination,
                amount,
            } => {
                let asset = EthereumAsset::Erc20(token.clone());
                let token_balance = self
                    .rpc
                    .balance(from.clone(), &asset, None)
                    .await
                    .map_err(rpc_error)?;
                let amount = requested_token_amount(amount.as_ref(), token_balance)?;
                if amount.is_zero() {
                    return Err(ChainError {
                        kind: ChainErrorKind::InsufficientFunds,
                        message: "Ethereum token collection would transfer zero units".to_owned(),
                    });
                }
                let transfer = EthereumTransferRequest::erc20(
                    signing_operation_id,
                    key,
                    from.clone(),
                    token,
                    destination,
                    amount.clone(),
                );
                let context = self.rpc.build_context(&transfer).await.map_err(rpc_error)?;
                self.validate_context(&context)?;
                let required = context
                    .max_fee_per_gas
                    .checked_mul_u64(context.gas_limit)
                    .ok_or_else(|| {
                        invalid_transaction("Ethereum maximum gas fee overflowed U256")
                    })?;
                let current = self
                    .rpc
                    .balance(from.clone(), &EthereumAsset::Native, None)
                    .await
                    .map_err(rpc_error)?;
                if current < required {
                    return Err(ChainError {
                        kind: ChainErrorKind::InsufficientFunds,
                        message: "Ethereum token address lacks native gas funds".to_owned(),
                    });
                }
                Ok((
                    transfer,
                    context,
                    EthereumCollectionAttribution {
                        address: from,
                        asset,
                        gross_debit: amount,
                    },
                ))
            }
        }
    }
}

impl<R: EthereumRpc> DepositAddressGenerator<Ethereum> for EthereumWallet<R> {
    fn generate_address<'a>(
        &'a self,
        request: EthereumGenerateAddress,
        keys: &'a dyn KeyProvisioner,
    ) -> BoxFuture<'a, Result<GeneratedAddress<EthereumAddress>, ChainError>> {
        Box::pin(async move {
            if request.chain_id != self.chain_id {
                return Err(invalid_public_key(format!(
                    "Ethereum address request uses chain ID {}, expected {}",
                    request.chain_id, self.chain_id
                )));
            }
            EthereumAddressGenerator
                .generate_address(request, keys)
                .await
        })
    }
}

impl<R: EthereumRpc> BalanceReader<Ethereum> for EthereumWallet<R> {
    fn balance<'a>(
        &'a self,
        address: &'a EthereumAddress,
        asset: &'a EthereumAsset,
    ) -> BoxFuture<'a, Result<Balance<Wei>, ChainError>> {
        Box::pin(async move {
            let confirmed = self
                .rpc
                .balance(address.clone(), asset, None)
                .await
                .map_err(rpc_error)?;
            Ok(Balance {
                spendable: confirmed.clone(),
                confirmed,
                pending: Wei::ZERO,
            })
        })
    }
}

impl<R: EthereumRpc> TransferBuilder<Ethereum> for EthereumWallet<R> {
    fn build_transfer<'a>(
        &'a self,
        request: EthereumTransferRequest,
    ) -> BoxFuture<'a, Result<UnsignedEthereumTransaction, ChainError>> {
        Box::pin(async move {
            let context = self.rpc.build_context(&request).await.map_err(rpc_error)?;
            self.validate_context(&context)?;
            crate::EthereumTransactionBuilder::build(&self.codec, request, context)
        })
    }
}

impl<R: EthereumRpc> TransactionSigner<Ethereum> for EthereumWallet<R> {
    fn sign_transaction<'a>(
        &'a self,
        transaction: UnsignedEthereumTransaction,
        signer: &'a dyn Signer,
    ) -> BoxFuture<'a, Result<EthereumSignedTransaction, ChainError>> {
        crate::EthereumTransactionSigning::sign(&self.codec, transaction, signer)
    }
}

impl<R: EthereumRpc> Broadcaster<Ethereum> for EthereumWallet<R> {
    fn broadcast<'a>(
        &'a self,
        transaction: EthereumSignedTransaction,
    ) -> BoxFuture<'a, Result<EthereumTransactionId, ChainError>> {
        Box::pin(async move { self.rpc.broadcast(transaction).await.map_err(rpc_error) })
    }
}

impl<R: EthereumRpc> TransactionReader<Ethereum> for EthereumWallet<R> {
    fn transaction<'a>(
        &'a self,
        id: &'a EthereumTransactionId,
    ) -> BoxFuture<'a, Result<Option<crate::EthereumReceipt>, ChainError>> {
        Box::pin(async move { self.rpc.receipt(id).await.map_err(rpc_error) })
    }
}

impl<R: EthereumRpc> Collector<Ethereum> for EthereumWallet<R> {
    fn requirements<'a>(
        &'a self,
        request: &'a EthereumCollectionRequest,
    ) -> BoxFuture<'a, Result<Vec<EthereumCollectionRequirement>, ChainError>> {
        Box::pin(async move {
            let EthereumCollectionRequest::Token {
                signing_operation_id,
                token,
                from,
                key,
                destination,
                amount,
            } = request
            else {
                return Ok(Vec::new());
            };
            let token_balance = self
                .rpc
                .balance(from.clone(), &EthereumAsset::Erc20(token.clone()), None)
                .await
                .map_err(rpc_error)?;
            let amount = requested_token_amount(amount.as_ref(), token_balance)?;
            if amount.is_zero() {
                return Ok(Vec::new());
            }
            let transfer = EthereumTransferRequest::erc20(
                signing_operation_id.clone(),
                key.clone(),
                from.clone(),
                token.clone(),
                destination.clone(),
                amount,
            );
            let context = self.rpc.build_context(&transfer).await.map_err(rpc_error)?;
            self.validate_context(&context)?;
            let required = context
                .max_fee_per_gas
                .checked_mul_u64(context.gas_limit)
                .ok_or_else(|| invalid_transaction("Ethereum maximum gas fee overflowed U256"))?;
            let current = self
                .rpc
                .balance(from.clone(), &EthereumAsset::Native, None)
                .await
                .map_err(rpc_error)?;
            let Some(deficit) = required.checked_sub(&current) else {
                return Ok(Vec::new());
            };
            if deficit.is_zero() {
                return Ok(Vec::new());
            }
            Ok(vec![EthereumCollectionRequirement::NativeGasBalance {
                address: from.clone(),
                current,
                required,
                deficit,
            }])
        })
    }

    fn collect<'a>(
        &'a self,
        request: EthereumCollectionRequest,
        signer: &'a dyn Signer,
    ) -> BoxFuture<
        'a,
        Result<
            CollectionSubmission<EthereumTransactionId, EthereumCollectionAttribution>,
            ChainError,
        >,
    > {
        Box::pin(async move {
            let prepared = self.prepare_collection(request, signer).await?;
            let transaction_id = self
                .rpc
                .broadcast(prepared.transaction)
                .await
                .map_err(rpc_error)?;
            Ok(CollectionSubmission {
                transaction_id,
                attribution: prepared.attribution,
            })
        })
    }
}

impl<R: EthereumRpc> WalletFactory<Ethereum> for EthereumWallet<R> {
    fn wallet_for<'a>(
        &'a self,
        _asset: &'a EthereumAsset,
    ) -> Result<&'a dyn WalletAdapter<Ethereum>, ChainError> {
        Ok(self)
    }
}

fn requested_token_amount(requested: Option<&Wei>, available: Wei) -> Result<Wei, ChainError> {
    match requested {
        Some(requested) if requested > &available => Err(ChainError {
            kind: ChainErrorKind::InsufficientFunds,
            message: "Ethereum token balance is lower than the requested collection amount"
                .to_owned(),
        }),
        Some(requested) => Ok(requested.clone()),
        None => Ok(available),
    }
}

fn rpc_error(error: SourceError) -> ChainError {
    ChainError {
        kind: if error.retryable {
            ChainErrorKind::RpcUnavailable
        } else {
            ChainErrorKind::Other
        },
        message: format!("Ethereum RPC operation failed: {error}"),
    }
}

fn invalid_transaction(message: impl Into<String>) -> ChainError {
    ChainError {
        kind: ChainErrorKind::InvalidTransaction,
        message: message.into(),
    }
}

fn raw_ethereum_public_key(public_key: &PublicKey) -> Result<&[u8], ChainError> {
    if public_key.curve != Curve::Secp256k1 {
        return Err(invalid_public_key(
            "Ethereum requires a secp256k1 public key",
        ));
    }

    match public_key.format {
        PublicKeyFormat::Raw if public_key.bytes.len() == 64 => Ok(&public_key.bytes),
        PublicKeyFormat::Uncompressed
            if public_key.bytes.len() == 65 && public_key.bytes[0] == 0x04 =>
        {
            Ok(&public_key.bytes[1..])
        }
        _ => Err(invalid_public_key(
            "Ethereum requires a 64-byte raw or 65-byte SEC1 public key",
        )),
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
    use alloy_primitives::keccak256;
    use futures_executor::block_on;
    use indexing::BlockRef;
    use signer_local::LocalSigner;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn rpc_error_does_not_turn_permanent_source_failures_into_retries() {
        let retryable = rpc_error(SourceError {
            message: "temporary".to_owned(),
            retryable: true,
        });
        let permanent = rpc_error(SourceError {
            message: "permanent".to_owned(),
            retryable: false,
        });

        assert_eq!(retryable.kind, ChainErrorKind::RpcUnavailable);
        assert_eq!(permanent.kind, ChainErrorKind::Other);
    }

    #[derive(Debug)]
    struct MockEthereumRpc {
        chain_id: u64,
        native_balance: Wei,
        token_balance: Wei,
        context: crate::EthereumBuildContext,
        broadcasts: AtomicUsize,
    }

    impl EthereumRpc for MockEthereumRpc {
        fn balance<'a>(
            &'a self,
            _address: EthereumAddress,
            asset: &'a EthereumAsset,
            _at: Option<BlockRef>,
        ) -> crate::BoxFuture<'a, Result<Wei, SourceError>> {
            let balance = match asset {
                EthereumAsset::Native => self.native_balance.clone(),
                EthereumAsset::Erc20(_) => self.token_balance.clone(),
            };
            Box::pin(async move { Ok(balance) })
        }

        fn nonce<'a>(
            &'a self,
            _address: EthereumAddress,
        ) -> crate::BoxFuture<'a, Result<u64, SourceError>> {
            Box::pin(async move { Ok(self.context.nonce) })
        }

        fn build_context<'a>(
            &'a self,
            _request: &'a EthereumTransferRequest,
        ) -> crate::BoxFuture<'a, Result<crate::EthereumBuildContext, SourceError>> {
            let context = self.context.clone();
            Box::pin(async move { Ok(context) })
        }

        fn receipt<'a>(
            &'a self,
            _id: &'a EthereumTransactionId,
        ) -> crate::BoxFuture<'a, Result<Option<crate::EthereumReceipt>, SourceError>> {
            Box::pin(async { Ok(None) })
        }

        fn broadcast<'a>(
            &'a self,
            transaction: EthereumSignedTransaction,
        ) -> crate::BoxFuture<'a, Result<EthereumTransactionId, SourceError>> {
            self.broadcasts.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move { Ok(transaction.id) })
        }
    }

    fn mock_rpc(native_balance: u128, token_balance: u128) -> MockEthereumRpc {
        MockEthereumRpc {
            chain_id: 31_337,
            native_balance: Wei::from_u128(native_balance),
            token_balance: Wei::from_u128(token_balance),
            context: crate::EthereumBuildContext {
                chain_id: 31_337,
                nonce: 2,
                gas_limit: 10,
                max_fee_per_gas: Wei::from_u128(2),
                max_priority_fee_per_gas: Wei::from_u128(1),
            },
            broadcasts: AtomicUsize::new(0),
        }
    }

    fn operation(value: &str) -> OperationId {
        OperationId::new(value).expect("test operation ID must be valid")
    }

    #[test]
    fn generates_address_from_an_ephemeral_key() {
        let keys = LocalSigner::ephemeral_for_testing();
        let generator = EthereumAddressGenerator;
        let request = EthereumGenerateAddress::new(
            31_337,
            operation("provision-ethereum-test-deposit"),
            "ethereum-test-deposit",
        );

        let generated = block_on(generator.generate_address(request, &keys))
            .expect("Ethereum address should be generated");
        let expected = Address::from_raw_public_key(&generated.public_key.bytes).into_array();

        assert_eq!(generated.address, EthereumAddress(expected));
        assert_eq!(generated.public_key.curve, Curve::Secp256k1);
        assert_eq!(generated.public_key.format, PublicKeyFormat::Raw);
        assert_eq!(generated.public_key.bytes.len(), 64);
    }

    #[test]
    fn generates_a_fresh_address_for_each_request() {
        let keys = LocalSigner::ephemeral_for_testing();
        let generator = EthereumAddressGenerator;

        let first = block_on(generator.generate_address(
            EthereumGenerateAddress::new(31_337, operation("provision-first"), "first"),
            &keys,
        ))
        .expect("first address should be generated");
        let second = block_on(generator.generate_address(
            EthereumGenerateAddress::new(31_337, operation("provision-second"), "second"),
            &keys,
        ))
        .expect("second address should be generated");

        assert_ne!(first.address, second.address);
        assert_ne!(first.key, second.key);
    }

    #[test]
    fn reports_token_gas_deficit() {
        let wallet = EthereumWallet::new(31_337, mock_rpc(12, 500));
        let request = EthereumCollectionRequest::Token {
            signing_operation_id: operation("requirements-token"),
            token: EthereumAddress([1; 20]),
            from: EthereumAddress([2; 20]),
            key: signer::KeyLocator::Identifier("token-source".to_owned()),
            destination: EthereumAddress([3; 20]),
            amount: None,
        };

        let requirements = block_on(wallet.requirements(&request))
            .expect("token collection requirements should be calculated");

        assert_eq!(
            requirements,
            vec![EthereumCollectionRequirement::NativeGasBalance {
                address: EthereumAddress([2; 20]),
                current: Wei::from_u128(12),
                required: Wei::from_u128(20),
                deficit: Wei::from_u128(8),
            }]
        );
    }

    #[test]
    fn rejects_token_collection_above_the_available_balance() {
        let wallet = EthereumWallet::new(31_337, mock_rpc(100, 500));
        let request = EthereumCollectionRequest::Token {
            signing_operation_id: operation("requirements-overdrawn-token"),
            token: EthereumAddress([1; 20]),
            from: EthereumAddress([2; 20]),
            key: signer::KeyLocator::Identifier("token-source".to_owned()),
            destination: EthereumAddress([3; 20]),
            amount: Some(Wei::from_u128(501)),
        };

        let error = block_on(wallet.requirements(&request))
            .expect_err("an overdrawn token collection should be rejected");

        assert_eq!(error.kind, ChainErrorKind::InsufficientFunds);
    }

    #[test]
    fn collects_one_token_transfer_and_returns_attribution() {
        let signer = LocalSigner::ephemeral_for_testing();
        let generated = block_on(EthereumAddressGenerator.generate_address(
            EthereumGenerateAddress::new(
                31_337,
                operation("provision-token-source"),
                "token-source",
            ),
            &signer,
        ))
        .expect("token source should be generated");
        let wallet = EthereumWallet::new(31_337, mock_rpc(100, 500));
        let source = generated.address.clone();
        let token = EthereumAddress([4; 20]);
        let submission = block_on(wallet.collect(
            EthereumCollectionRequest::Token {
                signing_operation_id: operation("sign-token-collection"),
                token: token.clone(),
                from: generated.address,
                key: generated.key,
                destination: EthereumAddress([5; 20]),
                amount: None,
            },
            &signer,
        ))
        .expect("token collection should succeed");

        assert_eq!(wallet.rpc().chain_id, 31_337);
        assert_eq!(wallet.rpc().broadcasts.load(Ordering::Relaxed), 1);
        assert_eq!(
            submission.attribution,
            vec![EthereumCollectionAttribution {
                address: source,
                asset: EthereumAsset::Erc20(token),
                gross_debit: Wei::from_u128(500),
            }]
        );
    }

    #[test]
    fn prepares_collection_without_broadcasting_the_signed_envelope() {
        let signer = LocalSigner::ephemeral_for_testing();
        let generated = block_on(EthereumAddressGenerator.generate_address(
            EthereumGenerateAddress::new(
                31_337,
                operation("provision-native-source"),
                "native-source",
            ),
            &signer,
        ))
        .expect("native source should be generated");
        let wallet = EthereumWallet::new(31_337, mock_rpc(100, 0));
        let source = generated.address.clone();
        let prepared = block_on(wallet.prepare_collection(
            EthereumCollectionRequest::Native {
                signing_operation_id: operation("sign-native-collection"),
                from: generated.address,
                key: generated.key,
                destination: EthereumAddress([6; 20]),
            },
            &signer,
        ))
        .expect("native collection should be prepared");

        assert_eq!(wallet.rpc().broadcasts.load(Ordering::Relaxed), 0);
        assert_eq!(
            prepared.transaction.id.0,
            keccak256(&prepared.transaction.envelope).0
        );
        assert_eq!(
            prepared.attribution,
            vec![EthereumCollectionAttribution {
                address: source,
                asset: EthereumAsset::Native,
                gross_debit: Wei::from_u128(80),
            }]
        );
    }
}
