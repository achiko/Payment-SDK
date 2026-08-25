use base::{Decimal, DecimalError, TransactionFuture};

use crate::{Address, Wei, erc20};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TransferRequest(Transfer);

/// Read-only, chain-native transfer intent exposed to transaction adapters.
///
/// The view contains only native or ERC-20 transfer fields. It deliberately
/// does not expose arbitrary transaction calldata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferIntent<'a> {
    Native {
        from: &'a Address,
        to: &'a Address,
        value: &'a Wei,
    },
    Erc20 {
        from: &'a Address,
        token: &'a Address,
        recipient: &'a Address,
        amount: &'a Wei,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Transfer {
    Native {
        from: Address,
        to: Address,
        value: Wei,
    },
    Erc20 {
        from: Address,
        token: Address,
        recipient: Address,
        amount: Wei,
    },
}

impl TransferRequest {
    pub fn native(from: Address, to: Address, value: Decimal) -> Result<Self, DecimalError> {
        Ok(Self::native_atomic(from, to, Wei::from_decimal(&value)?))
    }

    #[must_use]
    pub fn native_atomic(from: Address, to: Address, value: Wei) -> Self {
        Self(Transfer::Native { from, to, value })
    }

    #[must_use]
    pub fn erc20(from: Address, token: Address, recipient: Address, amount: Wei) -> Self {
        Self(Transfer::Erc20 {
            from,
            token,
            recipient,
            amount,
        })
    }

    /// Returns the exact typed intent for an external transaction adapter.
    #[must_use]
    pub const fn intent(&self) -> TransferIntent<'_> {
        match &self.0 {
            Transfer::Native { from, to, value } => TransferIntent::Native { from, to, value },
            Transfer::Erc20 {
                from,
                token,
                recipient,
                amount,
            } => TransferIntent::Erc20 {
                from,
                token,
                recipient,
                amount,
            },
        }
    }

    pub(crate) fn from(&self) -> &Address {
        match &self.0 {
            Transfer::Native { from, .. } | Transfer::Erc20 { from, .. } => from,
        }
    }

    pub(crate) fn to(&self) -> &Address {
        match &self.0 {
            Transfer::Native { to, .. } => to,
            Transfer::Erc20 { token, .. } => token,
        }
    }

    pub(crate) fn value(&self) -> Wei {
        match &self.0 {
            Transfer::Native { value, .. } => value.clone(),
            Transfer::Erc20 { .. } => Wei::ZERO,
        }
    }

    pub(crate) fn input(&self) -> Vec<u8> {
        match &self.0 {
            Transfer::Native { .. } => Vec::new(),
            Transfer::Erc20 {
                recipient, amount, ..
            } => erc20::transfer(recipient, amount),
        }
    }

    pub(crate) fn erc20_transfer(&self) -> Option<(&Address, &Wei)> {
        match &self.0 {
            Transfer::Native { .. } => None,
            Transfer::Erc20 { token, amount, .. } => Some((token, amount)),
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
