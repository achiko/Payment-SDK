//! Stateless Wallet Service application facade.
//!
//! The service composes a chain-specific [`chain_contract::WalletFactory`]
//! with injected provisioning and signing backends. Durable workflow state,
//! transport authentication, and request persistence stay outside this crate.

pub mod api;
pub mod bitcoin_api;
mod bitcoin_ix;
mod bitcoin_operations;

pub use bitcoin_ix::{BitcoinIxClient, BitcoinIxClientConfig, BitcoinIxReadiness};
pub use bitcoin_operations::{BitcoinOperationPolicy, BitcoinOperations};

use chain_contract::{
    Balance, Chain, ChainError, CollectionSubmission, GeneratedAddress, WalletFactory,
};
use signer::{KeyProvisioner, Signer};
use std::marker::PhantomData;

/// One stateless Wallet Service instance for a concrete chain.
#[derive(Debug)]
pub struct WalletService<C, F, K, S> {
    wallets: F,
    keys: K,
    signer: S,
    chain: PhantomData<C>,
}

/// Production-facing object-safe Ethereum operation adapter used by the HTTP
/// process. It remains stateless: RPC and custody clients are injected and no
/// database or durable workflow state is available here.
pub struct EthereumOperations<R, K, S> {
    service: WalletService<chain_ethereum::Ethereum, chain_ethereum::EthereumWallet<R>, K, S>,
}

impl<R, K, S> EthereumOperations<R, K, S>
where
    R: chain_ethereum::EthereumRpc,
    K: KeyProvisioner,
    S: Signer,
{
    #[must_use]
    pub const fn new(
        service: WalletService<chain_ethereum::Ethereum, chain_ethereum::EthereumWallet<R>, K, S>,
    ) -> Self {
        Self { service }
    }
}

impl<R, K, S> api::EthereumWalletOperations for EthereumOperations<R, K, S>
where
    R: chain_ethereum::EthereumRpc + 'static,
    K: KeyProvisioner + 'static,
    S: Signer + 'static,
{
    fn generate_address<'a>(
        &'a self,
        asset: chain_ethereum::EthereumAsset,
        operation_id: signer::OperationId,
        key_purpose: String,
    ) -> api::OperationFuture<
        'a,
        Result<GeneratedAddress<chain_ethereum::EthereumAddress>, ChainError>,
    > {
        Box::pin(async move {
            self.service
                .generate_address(
                    &asset,
                    chain_ethereum::EthereumGenerateAddress::new(
                        self.service.wallets().chain_id(),
                        operation_id,
                        key_purpose,
                    ),
                )
                .await
        })
    }

    fn balance<'a>(
        &'a self,
        asset: chain_ethereum::EthereumAsset,
        address: chain_ethereum::EthereumAddress,
    ) -> api::OperationFuture<'a, Result<Balance<chain_ethereum::Wei>, ChainError>> {
        Box::pin(async move { self.service.balance(&asset, &address).await })
    }

    fn sign_transfer<'a>(
        &'a self,
        asset: chain_ethereum::EthereumAsset,
        request: chain_ethereum::EthereumTransferRequest,
    ) -> api::OperationFuture<'a, Result<chain_ethereum::EthereumSignedTransaction, ChainError>>
    {
        Box::pin(async move {
            validate_transfer_asset(&asset, &request)?;
            let unsigned = self.service.build_transfer(&asset, request).await?;
            self.service.sign_transaction(&asset, unsigned).await
        })
    }

    fn collection_requirements<'a>(
        &'a self,
        asset: chain_ethereum::EthereumAsset,
        request: chain_ethereum::EthereumCollectionRequest,
    ) -> api::OperationFuture<
        'a,
        Result<Vec<chain_ethereum::EthereumCollectionRequirement>, ChainError>,
    > {
        Box::pin(async move {
            validate_collection_asset(&asset, &request)?;
            self.service.collection_requirements(&asset, &request).await
        })
    }

    fn prepare_collection<'a>(
        &'a self,
        asset: chain_ethereum::EthereumAsset,
        request: chain_ethereum::EthereumCollectionRequest,
    ) -> api::OperationFuture<'a, Result<chain_ethereum::EthereumPreparedCollection, ChainError>>
    {
        Box::pin(async move {
            validate_collection_asset(&asset, &request)?;
            self.service
                .wallets()
                .prepare_collection(request, self.service.signer())
                .await
        })
    }

    fn broadcast<'a>(
        &'a self,
        transaction: chain_ethereum::EthereumSignedTransaction,
    ) -> api::OperationFuture<'a, Result<chain_ethereum::EthereumTransactionId, ChainError>> {
        Box::pin(async move {
            self.service
                .broadcast(&chain_ethereum::EthereumAsset::Native, transaction)
                .await
        })
    }

    fn receipt<'a>(
        &'a self,
        transaction_id: chain_ethereum::EthereumTransactionId,
    ) -> api::OperationFuture<'a, Result<Option<chain_ethereum::EthereumReceipt>, ChainError>> {
        Box::pin(async move {
            self.service
                .transaction(&chain_ethereum::EthereumAsset::Native, &transaction_id)
                .await
        })
    }
}

fn validate_transfer_asset(
    asset: &chain_ethereum::EthereumAsset,
    request: &chain_ethereum::EthereumTransferRequest,
) -> Result<(), ChainError> {
    match asset {
        chain_ethereum::EthereumAsset::Native if request.data.is_empty() => Ok(()),
        chain_ethereum::EthereumAsset::Erc20(token)
            if request.to.as_ref() == Some(token) && request.data.len() == 68 =>
        {
            Ok(())
        }
        _ => Err(ChainError {
            kind: chain_contract::ChainErrorKind::InvalidTransaction,
            message: "Ethereum transfer asset does not match its chain-native request".to_owned(),
        }),
    }
}

fn validate_collection_asset(
    asset: &chain_ethereum::EthereumAsset,
    request: &chain_ethereum::EthereumCollectionRequest,
) -> Result<(), ChainError> {
    match (asset, request) {
        (
            chain_ethereum::EthereumAsset::Native,
            chain_ethereum::EthereumCollectionRequest::Native { .. },
        ) => Ok(()),
        (
            chain_ethereum::EthereumAsset::Erc20(asset_token),
            chain_ethereum::EthereumCollectionRequest::Token { token, .. },
        ) if asset_token == token => Ok(()),
        _ => Err(ChainError {
            kind: chain_contract::ChainErrorKind::InvalidTransaction,
            message: "Ethereum collection asset does not match its chain-native request".to_owned(),
        }),
    }
}

impl<C, F, K, S> WalletService<C, F, K, S>
where
    C: Chain,
    F: WalletFactory<C>,
    K: KeyProvisioner,
    S: Signer,
{
    #[must_use]
    pub const fn new(wallets: F, keys: K, signer: S) -> Self {
        Self {
            wallets,
            keys,
            signer,
            chain: PhantomData,
        }
    }

    #[must_use]
    pub const fn wallets(&self) -> &F {
        &self.wallets
    }

    #[must_use]
    pub const fn key_provisioner(&self) -> &K {
        &self.keys
    }

    #[must_use]
    pub const fn signer(&self) -> &S {
        &self.signer
    }

    pub async fn generate_address(
        &self,
        asset: &C::Asset,
        request: C::GenerateAddressRequest,
    ) -> Result<GeneratedAddress<C::Address>, ChainError> {
        self.wallets
            .wallet_for(asset)?
            .generate_address(request, &self.keys)
            .await
    }

    pub async fn balance(
        &self,
        asset: &C::Asset,
        address: &C::Address,
    ) -> Result<Balance<C::Amount>, ChainError> {
        self.wallets
            .wallet_for(asset)?
            .balance(address, asset)
            .await
    }

    pub async fn build_transfer(
        &self,
        asset: &C::Asset,
        request: C::TransferRequest,
    ) -> Result<C::UnsignedTransaction, ChainError> {
        self.wallets
            .wallet_for(asset)?
            .build_transfer(request)
            .await
    }

    pub async fn sign_transaction(
        &self,
        asset: &C::Asset,
        transaction: C::UnsignedTransaction,
    ) -> Result<C::SignedTransaction, ChainError> {
        self.wallets
            .wallet_for(asset)?
            .sign_transaction(transaction, &self.signer)
            .await
    }

    pub async fn broadcast(
        &self,
        asset: &C::Asset,
        transaction: C::SignedTransaction,
    ) -> Result<C::TransactionId, ChainError> {
        self.wallets.wallet_for(asset)?.broadcast(transaction).await
    }

    pub async fn transaction(
        &self,
        asset: &C::Asset,
        transaction_id: &C::TransactionId,
    ) -> Result<Option<C::Receipt>, ChainError> {
        self.wallets
            .wallet_for(asset)?
            .transaction(transaction_id)
            .await
    }

    pub async fn collection_requirements(
        &self,
        asset: &C::Asset,
        request: &C::CollectionRequest,
    ) -> Result<Vec<C::CollectionRequirement>, ChainError> {
        self.wallets.wallet_for(asset)?.requirements(request).await
    }

    pub async fn collect(
        &self,
        asset: &C::Asset,
        request: C::CollectionRequest,
    ) -> Result<CollectionSubmission<C::TransactionId, C::CollectionAttribution>, ChainError> {
        self.wallets
            .wallet_for(asset)?
            .collect(request, &self.signer)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chain_ethereum::{
        Ethereum, EthereumAddress, EthereumAsset, EthereumBuildContext, EthereumGenerateAddress,
        EthereumReceipt, EthereumRpc, EthereumSignedTransaction, EthereumTransactionId,
        EthereumTransferRequest, EthereumWallet, Wei,
    };
    use futures_executor::block_on;
    use indexing::{BlockRef, SourceError};
    use signer_local::LocalSigner;
    use std::sync::Arc;

    fn operation(value: &str) -> signer::OperationId {
        signer::OperationId::new(value).expect("test operation ID must be valid")
    }

    #[derive(Debug)]
    struct MockEthereumRpc;

    impl EthereumRpc for MockEthereumRpc {
        fn balance<'a>(
            &'a self,
            _address: EthereumAddress,
            _asset: &'a EthereumAsset,
            _at: Option<BlockRef>,
        ) -> chain_ethereum::BoxFuture<'a, Result<Wei, SourceError>> {
            Box::pin(async { Ok(Wei::ZERO) })
        }

        fn nonce<'a>(
            &'a self,
            _address: EthereumAddress,
        ) -> chain_ethereum::BoxFuture<'a, Result<u64, SourceError>> {
            Box::pin(async { Ok(0) })
        }

        fn build_context<'a>(
            &'a self,
            _request: &'a EthereumTransferRequest,
        ) -> chain_ethereum::BoxFuture<'a, Result<EthereumBuildContext, SourceError>> {
            Box::pin(async {
                Ok(EthereumBuildContext {
                    chain_id: 31_337,
                    nonce: 0,
                    gas_limit: 21_000,
                    max_fee_per_gas: Wei::from_u128(2),
                    max_priority_fee_per_gas: Wei::from_u128(1),
                })
            })
        }

        fn receipt<'a>(
            &'a self,
            _id: &'a EthereumTransactionId,
        ) -> chain_ethereum::BoxFuture<'a, Result<Option<EthereumReceipt>, SourceError>> {
            Box::pin(async { Ok(None) })
        }

        fn broadcast<'a>(
            &'a self,
            transaction: EthereumSignedTransaction,
        ) -> chain_ethereum::BoxFuture<'a, Result<EthereumTransactionId, SourceError>> {
            Box::pin(async move { Ok(transaction.id) })
        }
    }

    #[test]
    fn shared_custody_provisions_builds_and_signs_through_the_service() {
        let custody = Arc::new(LocalSigner::ephemeral_for_testing());
        let service = WalletService::<Ethereum, _, _, _>::new(
            EthereumWallet::new(31_337, MockEthereumRpc),
            Arc::clone(&custody),
            custody,
        );
        let asset = EthereumAsset::Native;
        let generated = block_on(service.generate_address(
            &asset,
            EthereumGenerateAddress::new(
                31_337,
                operation("provision-service-sender"),
                "service-sender",
            ),
        ))
        .expect("service should generate an Ethereum address");
        let unsigned = block_on(service.build_transfer(
            &asset,
            EthereumTransferRequest {
                signing_operation_id: operation("sign-service-transfer"),
                key: generated.key,
                from: generated.address,
                to: Some(EthereumAddress([9; 20])),
                value: Wei::from_u128(1),
                data: Vec::new(),
            },
        ))
        .expect("service should build an Ethereum transfer");
        let signed = block_on(service.sign_transaction(&asset, unsigned))
            .expect("the same custody backend should sign its provisioned key");

        assert_eq!(signed.envelope[0], 0x02);
    }
}
