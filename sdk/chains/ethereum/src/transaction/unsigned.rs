use crate::{BoxFuture, EthereumAddress, Wei};
use chain_contract::ChainError;
use signer::KeyLocator;
use signer::Signer;

use super::EthereumSignedTransaction;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsignedEthereumTransaction {
    pub key: KeyLocator,
    pub chain_id: u64,
    pub nonce: u64,
    pub from: EthereumAddress,
    pub to: Option<EthereumAddress>,
    pub value: Wei,
    pub input: Vec<u8>,
    pub gas_limit: u64,
    pub max_fee_per_gas: Wei,
    pub max_priority_fee_per_gas: Wei,
}

pub trait EthereumTransactionSigning: Send + Sync {
    fn sign<'a>(
        &'a self,
        transaction: UnsignedEthereumTransaction,
        signer: &'a dyn Signer,
    ) -> BoxFuture<'a, Result<EthereumSignedTransaction, ChainError>>;
}
