use crate::{EthereumAddress, Wei};
use signer::KeyLocator;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumTransferRequest {
    pub key: KeyLocator,
    pub from: EthereumAddress,
    pub to: Option<EthereumAddress>,
    pub value: Wei,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumBuildContext {
    pub chain_id: u64,
    pub nonce: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: Wei,
    pub max_priority_fee_per_gas: Wei,
}

pub trait EthereumTransactionBuilder: Send + Sync {
    fn build(
        &self,
        request: EthereumTransferRequest,
        context: EthereumBuildContext,
    ) -> Result<super::UnsignedEthereumTransaction, chain_contract::ChainError>;
}
