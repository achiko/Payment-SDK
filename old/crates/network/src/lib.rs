//! Network transaction building, signing, wallet, and routing contracts.

use std::{error::Error, fmt, hash::Hash};

use signer::Signer;

pub use primitives::{Address, Signature, TxHash};

/// Associates one chain with its request, unsigned transaction, and envelope.
pub trait Chain: Clone + fmt::Debug + Eq + Hash + Sized + 'static {
    type TransactionRequest: TransactionRequest<Chain = Self>;
    type UnsignedTx: UnsignedTransaction<Chain = Self>;
    type SignedTxEnvelope: SignedTxEnvelope<Chain = Self>;

    const NAME: &'static str;

    fn mock_tx_hash() -> TxHash<Self>;
}

/// A request that chain fillers complete before a wallet builds it.
pub trait TransactionRequest: fmt::Debug + Sized {
    type Chain: Chain<TransactionRequest = Self>;

    fn sender(&self) -> Option<&Address<Self::Chain>>;
    fn set_sender(&mut self, sender: Address<Self::Chain>);
    fn steps(&self) -> &[&'static str];
    fn push_step(&mut self, step: &'static str);
    fn build(self) -> <Self::Chain as Chain>::UnsignedTx;
}

/// A built, unsigned value accepted by a transaction signer.
pub trait UnsignedTransaction: fmt::Debug + Sized {
    type Chain: Chain<UnsignedTx = Self>;

    fn message(&self) -> &'static str;
}

/// A signed value accepted by a provider for one chain.
pub trait SignedTxEnvelope: fmt::Debug + Sized {
    type Chain: Chain<SignedTxEnvelope = Self>;

    fn message(&self) -> &'static str;
}

/// Adds chain-specific request fields such as mock fees, nonces, or inputs.
pub trait TransactionFiller<C: Chain> {
    fn fill(&self, request: C::TransactionRequest) -> C::TransactionRequest;
}

/// Signs a built transaction for exactly one network.
pub trait TxSigner<C: Chain> {
    fn address(&self) -> &Address<C>;
    fn sign(&self, transaction: C::UnsignedTx) -> C::SignedTxEnvelope;
}

/// Marker for an adapter supporting both credential and transaction signing.
pub trait FullSigner<C: Chain>: Signer<Signature = Signature<C>> + TxSigner<C> {}

impl<C, S> FullSigner<C> for S
where
    C: Chain,
    S: Signer<Signature = Signature<C>> + TxSigner<C>,
{
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalletError {
    SignerNotFound { address: String },
}

impl fmt::Display for WalletError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SignerNotFound { address } => {
                write!(formatter, "no signer registered for {address}")
            }
        }
    }
}

impl Error for WalletError {}

/// Routes a request or unsigned transaction to a registered signer.
pub trait NetworkWallet<C: Chain> {
    fn default_signer_address(&self) -> &Address<C>;
    fn has_signer_for(&self, address: &Address<C>) -> bool;
    fn signer_addresses(&self) -> Vec<&Address<C>>;

    fn sign_transaction_from(
        &self,
        address: &Address<C>,
        transaction: C::UnsignedTx,
    ) -> Result<C::SignedTxEnvelope, WalletError>;

    fn sign_request(
        &self,
        mut request: C::TransactionRequest,
    ) -> Result<C::SignedTxEnvelope, WalletError> {
        let sender = request
            .sender()
            .cloned()
            .unwrap_or_else(|| self.default_signer_address().clone());

        if request.sender().is_none() {
            request.set_sender(sender.clone());
        }

        self.sign_transaction_from(&sender, request.build())
    }
}

/// Converts either a signer adapter or an existing wallet into a network wallet.
pub trait IntoWallet<C: Chain> {
    type Wallet: NetworkWallet<C>;

    fn into_wallet(self) -> Self::Wallet;
}
