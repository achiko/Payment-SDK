//! Ethereum network transaction models, fillers, and wallet routing.

use std::{collections::HashMap, sync::Arc};

use network::{
    Address, Chain, IntoWallet, NetworkWallet, Signature, SignedTxEnvelope, TransactionFiller,
    TransactionRequest, TxHash, TxSigner, UnsignedTransaction, WalletError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ethereum;

impl Chain for Ethereum {
    type TransactionRequest = EthereumTransactionRequest;
    type UnsignedTx = EthereumUnsignedTx;
    type SignedTxEnvelope = EthereumTxEnvelope;

    const NAME: &'static str = "ethereum";

    fn mock_tx_hash() -> TxHash<Self> {
        TxHash::new("0xmock-ethereum-hash")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthereumTransactionRequest {
    pub from: Option<Address<Ethereum>>,
    pub to: Address<Ethereum>,
    pub value_wei: u128,
    steps: Vec<&'static str>,
}

impl EthereumTransactionRequest {
    #[must_use]
    pub fn new() -> Self {
        Self::transfer(Address::new("0xreceiver"), 1_000)
    }

    #[must_use]
    pub fn transfer(to: Address<Ethereum>, value_wei: u128) -> Self {
        Self {
            from: None,
            to,
            value_wei,
            steps: vec!["ethereum transaction created"],
        }
    }

    #[must_use]
    pub fn from(mut self, sender: Address<Ethereum>) -> Self {
        self.from = Some(sender);
        self
    }
}

impl Default for EthereumTransactionRequest {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionRequest for EthereumTransactionRequest {
    type Chain = Ethereum;

    fn sender(&self) -> Option<&Address<Ethereum>> {
        self.from.as_ref()
    }

    fn set_sender(&mut self, sender: Address<Ethereum>) {
        self.from = Some(sender);
    }

    fn steps(&self) -> &[&'static str] {
        &self.steps
    }

    fn push_step(&mut self, step: &'static str) {
        self.steps.push(step);
    }

    fn build(self) -> EthereumUnsignedTx {
        EthereumUnsignedTx { request: self }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthereumUnsignedTx {
    request: EthereumTransactionRequest,
}

impl EthereumUnsignedTx {
    #[must_use]
    pub const fn request(&self) -> &EthereumTransactionRequest {
        &self.request
    }
}

impl UnsignedTransaction for EthereumUnsignedTx {
    type Chain = Ethereum;

    fn message(&self) -> &'static str {
        "ethereum unsigned transaction built"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthereumTxEnvelope {
    transaction: EthereumUnsignedTx,
    signature: Signature<Ethereum>,
}

impl EthereumTxEnvelope {
    #[must_use]
    pub const fn new(transaction: EthereumUnsignedTx, signature: Signature<Ethereum>) -> Self {
        Self {
            transaction,
            signature,
        }
    }

    #[must_use]
    pub const fn transaction(&self) -> &EthereumUnsignedTx {
        &self.transaction
    }

    #[must_use]
    pub const fn signature(&self) -> &Signature<Ethereum> {
        &self.signature
    }
}

impl SignedTxEnvelope for EthereumTxEnvelope {
    type Chain = Ethereum;

    fn message(&self) -> &'static str {
        "ethereum transaction signed"
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EthereumFiller;

impl TransactionFiller<Ethereum> for EthereumFiller {
    fn fill(&self, mut request: EthereumTransactionRequest) -> EthereumTransactionRequest {
        request.push_step("nonce added");
        request.push_step("ethereum fee added");
        request
    }
}

type DynEthereumSigner = dyn TxSigner<Ethereum> + Send + Sync;

#[derive(Clone)]
pub struct EthereumWallet {
    default: Address<Ethereum>,
    signers: HashMap<Address<Ethereum>, Arc<DynEthereumSigner>>,
}

impl EthereumWallet {
    #[must_use]
    pub fn new<S>(signer: S) -> Self
    where
        S: TxSigner<Ethereum> + Send + Sync + 'static,
    {
        let default = signer.address().clone();
        let mut signers: HashMap<_, Arc<DynEthereumSigner>> = HashMap::new();
        signers.insert(default.clone(), Arc::new(signer));
        Self { default, signers }
    }

    pub fn register_signer<S>(&mut self, signer: S)
    where
        S: TxSigner<Ethereum> + Send + Sync + 'static,
    {
        self.signers
            .insert(signer.address().clone(), Arc::new(signer));
    }

    pub fn register_default_signer<S>(&mut self, signer: S)
    where
        S: TxSigner<Ethereum> + Send + Sync + 'static,
    {
        self.default = signer.address().clone();
        self.register_signer(signer);
    }

    pub fn set_default_signer(&mut self, address: &Address<Ethereum>) -> Result<(), WalletError> {
        if !self.signers.contains_key(address) {
            return Err(WalletError::SignerNotFound {
                address: address.to_string(),
            });
        }
        self.default = address.clone();
        Ok(())
    }

    #[must_use]
    pub fn signer_by_address(&self, address: &Address<Ethereum>) -> Option<&DynEthereumSigner> {
        self.signers.get(address).map(Arc::as_ref)
    }
}

impl NetworkWallet<Ethereum> for EthereumWallet {
    fn default_signer_address(&self) -> &Address<Ethereum> {
        &self.default
    }

    fn has_signer_for(&self, address: &Address<Ethereum>) -> bool {
        self.signers.contains_key(address)
    }

    fn signer_addresses(&self) -> Vec<&Address<Ethereum>> {
        self.signers.keys().collect()
    }

    fn sign_transaction_from(
        &self,
        address: &Address<Ethereum>,
        transaction: EthereumUnsignedTx,
    ) -> Result<EthereumTxEnvelope, WalletError> {
        let signer =
            self.signer_by_address(address)
                .ok_or_else(|| WalletError::SignerNotFound {
                    address: address.to_string(),
                })?;
        Ok(signer.sign(transaction))
    }
}

impl IntoWallet<Ethereum> for EthereumWallet {
    type Wallet = Self;

    fn into_wallet(self) -> Self::Wallet {
        self
    }
}

impl std::fmt::Debug for EthereumWallet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EthereumWallet")
            .field("default", &self.default)
            .field("signer_count", &self.signers.len())
            .finish()
    }
}
