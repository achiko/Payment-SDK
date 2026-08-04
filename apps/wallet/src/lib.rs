//! Stateless Wallet Service application facade.
//!
//! The service composes a chain-specific [`chain_contract::WalletFactory`]
//! with injected provisioning and signing backends. Durable workflow state,
//! transport authentication, and request persistence stay outside this crate.

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
            EthereumGenerateAddress::new(31_337, "service-sender"),
        ))
        .expect("service should generate an Ethereum address");
        let unsigned = block_on(service.build_transfer(
            &asset,
            EthereumTransferRequest {
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
