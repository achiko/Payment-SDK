use alloy_primitives::keccak256;

use crate::{EthereumAddress, Wei};
use signer::{KeyLocator, OperationId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EthereumTransferRequest {
    pub signing_operation_id: OperationId,
    pub key: KeyLocator,
    pub from: EthereumAddress,
    pub to: Option<EthereumAddress>,
    pub value: Wei,
    pub data: Vec<u8>,
}

impl EthereumTransferRequest {
    #[must_use]
    pub fn native(
        signing_operation_id: OperationId,
        key: KeyLocator,
        from: EthereumAddress,
        to: EthereumAddress,
        value: Wei,
    ) -> Self {
        Self {
            signing_operation_id,
            key,
            from,
            to: Some(to),
            value,
            data: Vec::new(),
        }
    }

    /// Builds canonical ERC-20 `transfer(address,uint256)` calldata in the
    /// Ethereum crate so transport adapters never own protocol encoding.
    #[must_use]
    pub fn erc20(
        signing_operation_id: OperationId,
        key: KeyLocator,
        from: EthereumAddress,
        token: EthereumAddress,
        recipient: EthereumAddress,
        amount: Wei,
    ) -> Self {
        let mut data = Vec::with_capacity(68);
        data.extend_from_slice(&keccak256("transfer(address,uint256)").0[..4]);
        data.extend_from_slice(&[0; 12]);
        data.extend_from_slice(&recipient.0);
        data.extend_from_slice(&amount.0);
        Self {
            signing_operation_id,
            key,
            from,
            to: Some(token),
            value: Wei::ZERO,
            data,
        }
    }
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
