//! Bitcoin network transaction models, fillers, and wallet routing.

use std::{collections::HashMap, sync::Arc};

use network::{
    Address, Chain, IntoWallet, NetworkWallet, Signature, SignedTxEnvelope, TransactionFiller,
    TransactionRequest, TxHash, TxSigner, UnsignedTransaction, WalletError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Bitcoin;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitcoinNetwork {
    Mainnet,
    Testnet,
    Regtest,
}

impl Chain for Bitcoin {
    type TransactionRequest = BitcoinTransactionRequest;
    type UnsignedTx = BitcoinUnsignedTx;
    type SignedTxEnvelope = BitcoinTxEnvelope;

    const NAME: &'static str = "bitcoin";

    fn mock_tx_hash() -> TxHash<Self> {
        TxHash::new("mock-bitcoin-txid")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoinTransactionRequest {
    pub from: Option<Address<Bitcoin>>,
    pub to: Address<Bitcoin>,
    pub amount_sats: u64,
    steps: Vec<&'static str>,
}

impl BitcoinTransactionRequest {
    #[must_use]
    pub fn new() -> Self {
        Self::transfer(Address::new("bc1qreceiver"), 25_000)
    }

    #[must_use]
    pub fn transfer(to: Address<Bitcoin>, amount_sats: u64) -> Self {
        Self {
            from: None,
            to,
            amount_sats,
            steps: vec!["bitcoin transaction created"],
        }
    }

    #[must_use]
    pub fn from(mut self, sender: Address<Bitcoin>) -> Self {
        self.from = Some(sender);
        self
    }
}

impl Default for BitcoinTransactionRequest {
    fn default() -> Self {
        Self::new()
    }
}

impl TransactionRequest for BitcoinTransactionRequest {
    type Chain = Bitcoin;

    fn sender(&self) -> Option<&Address<Bitcoin>> {
        self.from.as_ref()
    }

    fn set_sender(&mut self, sender: Address<Bitcoin>) {
        self.from = Some(sender);
    }

    fn steps(&self) -> &[&'static str] {
        &self.steps
    }

    fn push_step(&mut self, step: &'static str) {
        self.steps.push(step);
    }

    fn build(self) -> BitcoinUnsignedTx {
        BitcoinUnsignedTx { request: self }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoinUnsignedTx {
    request: BitcoinTransactionRequest,
}

impl BitcoinUnsignedTx {
    #[must_use]
    pub const fn request(&self) -> &BitcoinTransactionRequest {
        &self.request
    }
}

impl UnsignedTransaction for BitcoinUnsignedTx {
    type Chain = Bitcoin;

    fn message(&self) -> &'static str {
        "bitcoin unsigned transaction built"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoinTxEnvelope {
    transaction: BitcoinUnsignedTx,
    signature: Signature<Bitcoin>,
}

impl BitcoinTxEnvelope {
    #[must_use]
    pub const fn new(transaction: BitcoinUnsignedTx, signature: Signature<Bitcoin>) -> Self {
        Self {
            transaction,
            signature,
        }
    }

    #[must_use]
    pub const fn transaction(&self) -> &BitcoinUnsignedTx {
        &self.transaction
    }

    #[must_use]
    pub const fn signature(&self) -> &Signature<Bitcoin> {
        &self.signature
    }
}

impl SignedTxEnvelope for BitcoinTxEnvelope {
    type Chain = Bitcoin;

    fn message(&self) -> &'static str {
        "bitcoin transaction signed"
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BitcoinFiller;

impl TransactionFiller<Bitcoin> for BitcoinFiller {
    fn fill(&self, mut request: BitcoinTransactionRequest) -> BitcoinTransactionRequest {
        request.push_step("UTXO inputs selected");
        request.push_step("bitcoin fee added");
        request
    }
}

type DynBitcoinSigner = dyn TxSigner<Bitcoin> + Send + Sync;

#[derive(Clone)]
pub struct BitcoinWallet {
    default: Address<Bitcoin>,
    signers: HashMap<Address<Bitcoin>, Arc<DynBitcoinSigner>>,
}

impl BitcoinWallet {
    #[must_use]
    pub fn new<S>(signer: S) -> Self
    where
        S: TxSigner<Bitcoin> + Send + Sync + 'static,
    {
        let default = signer.address().clone();
        let mut signers: HashMap<_, Arc<DynBitcoinSigner>> = HashMap::new();
        signers.insert(default.clone(), Arc::new(signer));
        Self { default, signers }
    }

    pub fn register_signer<S>(&mut self, signer: S)
    where
        S: TxSigner<Bitcoin> + Send + Sync + 'static,
    {
        self.signers
            .insert(signer.address().clone(), Arc::new(signer));
    }

    pub fn register_default_signer<S>(&mut self, signer: S)
    where
        S: TxSigner<Bitcoin> + Send + Sync + 'static,
    {
        self.default = signer.address().clone();
        self.register_signer(signer);
    }

    pub fn set_default_signer(&mut self, address: &Address<Bitcoin>) -> Result<(), WalletError> {
        if !self.signers.contains_key(address) {
            return Err(WalletError::SignerNotFound {
                address: address.to_string(),
            });
        }
        self.default = address.clone();
        Ok(())
    }

    #[must_use]
    pub fn signer_by_address(&self, address: &Address<Bitcoin>) -> Option<&DynBitcoinSigner> {
        self.signers.get(address).map(Arc::as_ref)
    }
}

impl NetworkWallet<Bitcoin> for BitcoinWallet {
    fn default_signer_address(&self) -> &Address<Bitcoin> {
        &self.default
    }

    fn has_signer_for(&self, address: &Address<Bitcoin>) -> bool {
        self.signers.contains_key(address)
    }

    fn signer_addresses(&self) -> Vec<&Address<Bitcoin>> {
        self.signers.keys().collect()
    }

    fn sign_transaction_from(
        &self,
        address: &Address<Bitcoin>,
        transaction: BitcoinUnsignedTx,
    ) -> Result<BitcoinTxEnvelope, WalletError> {
        let signer =
            self.signer_by_address(address)
                .ok_or_else(|| WalletError::SignerNotFound {
                    address: address.to_string(),
                })?;
        Ok(signer.sign(transaction))
    }
}

impl IntoWallet<Bitcoin> for BitcoinWallet {
    type Wallet = Self;

    fn into_wallet(self) -> Self::Wallet {
        self
    }
}

impl std::fmt::Debug for BitcoinWallet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BitcoinWallet")
            .field("default", &self.default)
            .field("signer_count", &self.signers.len())
            .finish()
    }
}
