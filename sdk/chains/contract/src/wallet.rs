use crate::{
    BoxFuture, Broadcaster, Chain, ChainError, Collector, TransactionReader, TransactionSigner,
    TransferBuilder,
};
use signer::{KeyLocator, KeyProvisioner, PublicKey};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Balance<A> {
    pub confirmed: A,
    pub pending: A,
    pub spendable: A,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedAddress<A> {
    pub address: A,
    /// Opaque ownership handle. The chain never chooses the signer implementation.
    pub key: KeyLocator,
    pub public_key: PublicKey,
}

/// Stateless chain-specific address derivation. Durable deposit creation and watch
/// registration remain application responsibilities.
pub trait DepositAddressGenerator<C: Chain>: Send + Sync {
    fn generate_address<'a>(
        &'a self,
        request: C::GenerateAddressRequest,
        keys: &'a dyn KeyProvisioner,
    ) -> BoxFuture<'a, Result<GeneratedAddress<C::Address>, ChainError>>;
}

pub trait BalanceReader<C: Chain>: Send + Sync {
    fn balance<'a>(
        &'a self,
        address: &'a C::Address,
        asset: &'a C::Asset,
    ) -> BoxFuture<'a, Result<Balance<C::Amount>, ChainError>>;
}

/// Optional WS facade over the small capabilities. Implementations remain
/// stateless; the facade exists for application composition, not code ownership.
pub trait WalletAdapter<C: Chain>:
    DepositAddressGenerator<C>
    + BalanceReader<C>
    + TransferBuilder<C>
    + TransactionSigner<C>
    + Broadcaster<C>
    + TransactionReader<C>
    + Collector<C>
{
}

impl<C, T> WalletAdapter<C> for T
where
    C: Chain,
    T: DepositAddressGenerator<C>
        + BalanceReader<C>
        + TransferBuilder<C>
        + TransactionSigner<C>
        + Broadcaster<C>
        + TransactionReader<C>
        + Collector<C>,
{
}

/// Per-chain runtime selection for assets supported by one wallet process.
pub trait WalletFactory<C: Chain>: Send + Sync {
    fn wallet_for<'a>(
        &'a self,
        asset: &'a C::Asset,
    ) -> Result<&'a dyn WalletAdapter<C>, ChainError>;
}
