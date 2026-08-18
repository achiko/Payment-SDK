use alloy_primitives::keccak256;
use base::{Decimal, DecimalError, TransactionFuture};

use crate::{Address, Wei};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TransferRequest {
    pub from: Address,
    pub to: Option<Address>,
    pub value: Wei,
    pub data: Vec<u8>,
}

impl TransferRequest {
    pub fn native(from: Address, to: Address, value: Decimal) -> Result<Self, DecimalError> {
        Ok(Self::native_atomic(from, to, Wei::from_decimal(&value)?))
    }

    #[must_use]
    pub fn native_atomic(from: Address, to: Address, value: Wei) -> Self {
        Self {
            from,
            to: Some(to),
            value,
            data: Vec::new(),
        }
    }

    /// Builds canonical ERC-20 `transfer(address,uint256)` calldata in the
    /// Ethereum crate so transport adapters never own protocol encoding.
    #[must_use]
    pub fn erc20(from: Address, token: Address, recipient: Address, amount: Wei) -> Self {
        let mut data = Vec::with_capacity(68);
        data.extend_from_slice(&keccak256("transfer(address,uint256)").0[..4]);
        data.extend_from_slice(&[0; 12]);
        data.extend_from_slice(&recipient.0);
        data.extend_from_slice(&amount.0);
        Self {
            from,
            to: Some(token),
            value: Wei::ZERO,
            data,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BuildContext {
    pub chain_id: u64,
    pub nonce: u64,
    pub gas_limit: u64,
    pub max_fee_per_gas: Wei,
    pub max_priority_fee_per_gas: Wei,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Builder {
    request: TransferRequest,
    context: BuildContext,
}

impl Builder {
    #[must_use]
    pub const fn new(request: TransferRequest, context: BuildContext) -> Self {
        Self { request, context }
    }

    pub fn build<'a>(
        &'a self,
    ) -> TransactionFuture<'a, Result<super::UnsignedTransaction, crate::ChainError>> {
        Box::pin(
            async move { super::operations::build(self.request.clone(), self.context.clone()) },
        )
    }

    pub fn sign<'a>(
        &'a self,
        signer: &'a dyn base::Signer,
    ) -> TransactionFuture<'a, Result<super::SignedTransaction, crate::ChainError>> {
        Box::pin(async move {
            let unsigned = super::operations::build(self.request.clone(), self.context.clone())?;
            super::operations::sign(unsigned, signer).await
        })
    }
}
